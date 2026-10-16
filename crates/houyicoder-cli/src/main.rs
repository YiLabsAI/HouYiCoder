//! Binary entry point. The composition root: builds the runner + protocol
//! server + client, sharing one Arc<Runner> between the server task and the
//! TUI, then hands the wired bundle to the TUI run loop.
//!
//! Flags:
//!   --project <path>   Pin the sandbox workspace to <path> so the agent's
//!                      bash lands in that repo (it can see + edit the code
//!                      it is developing). Overrides the walk-up resolution.
//!   --acp              Launch the Agent Client Protocol server over stdio
//!                      instead of the TUI. A stock client connects via the
//!                      pipe; frames are NDJSON JSON-RPC lines on stdin,
//!                      replies + notifications on stdout. The session id
//!                      is printed to stderr so the client prompts against
//!                      the right id (stdout stays clean for the frame
//!                      stream). Single-session per process; multi-session
//!                      detach + reconnect across processes is a later cut.
//!
//! Without --project, the runner resolves the workspace by walking up from
//! the process cwd to the nearest workspace manifest, so launching from
//! anywhere inside a repo still pins to its root. If no manifest is found
//! the sandbox falls back to an isolated tempdir + a stderr warning (never
//! silently uses the home dir as the workspace — that left the agent unable
//! to see the repo).

use std::env;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(unix)]
use std::sync::mpsc::channel;

use houyicoder_service::composition::host_lifecycle::HostLifecycle;
use houyicoder_service::composition::session_log_root;
#[cfg(unix)]
use houyicoder_tui::agent_message::SessionMessage;
use houyicoder_tui::composition::{FrontendStartup, RunnerBundle, shared_runtime};

mod cli_args;
mod composition;
#[cfg(unix)]
mod detach;
mod export_view;
mod local_host;
mod session;
mod trajectory;

pub(crate) use cli_args::{CliCommand, parse_args, print_help};
pub(crate) use composition::{WiredBundle, assemble_bundle, build_bundle, build_bundle_for_resume};
pub(crate) use local_host::{HostLease, HostLeaseScope, HostLeaseSlot, LeaseScopeGuard};

use crate::session::cleanup::run_cleanup;
use crate::session::housekeeping::fire_after_bundle;
use crate::session::lock::{LockError, SessionLock};
use crate::session::resume_bundle::build_bundle_for_fork;
use crate::session::resume_bundle::build_bundle_for_resume_sid;
use futures::StreamExt;
use futures::channel::mpsc;
#[cfg(unix)]
use houyicoder_client::{Client, UdsTransport};
use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_permission::FileRuleStore;
#[cfg(unix)]
use houyicoder_permission::ModeGate;
use houyicoder_protocol::acpx::AcpxCapabilities;
#[cfg(unix)]
use houyicoder_protocol::framing::MAX_MESSAGE_BYTES;
#[cfg(unix)]
use houyicoder_service::CancellationToken;
use houyicoder_service::acp::adapter::AcpAdapter;
use houyicoder_service::acp::carrier::AcpIo;
use houyicoder_service::acp::server::AcpServer;
use houyicoder_service::composition::BuildRunnerOptions;
use houyicoder_service::composition::ProviderResolution;
#[cfg(unix)]
use houyicoder_service::composition::SessionHost;
use houyicoder_service::composition::build_runner;
use houyicoder_service::composition::latest_session_sid;
use houyicoder_service::composition::resolve_provider;
use houyicoder_service::composition::workspace_cwd;
use houyicoder_service::diagnostics::install;
use houyicoder_service::lifecycle::SessionLeaseStore;
#[cfg(unix)]
use houyicoder_service::server::EventSequencer;
#[cfg(unix)]
use houyicoder_service::uds;
use houyicoder_tui::app::run_with_runner;
#[cfg(unix)]
use std::fs::remove_file;
use std::io::stdin;
use std::io::stdout;
use std::io::{BufRead, Write};
#[cfg(unix)]
use std::process::id;
use tokio::runtime::Builder;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = match parse_args(args) {
        Ok(cmd) => cmd,
        Err(msg) => {
            eprintln!("{msg}");
            exit(2);
        }
    };
    // Install the process-wide diagnostic sink before any server or bundle
    // is built. Starts disabled: a normal run pays a filter check per call
    // site and nothing else. The sink is file-backed because the TUI runs in
    // the alternate screen, which does not capture stderr — a print macro
    // from library code would land in the input box. A /debug wire command
    // raises the level at runtime; the handle is stored process-wide so the
    // server (in-proc, UDS, or ACP) reads it at construction. A failure to
    // install is not fatal: the /debug command will report the sink is
    // absent, and the session proceeds without a diagnostic channel.
    let log_path = env::current_dir()
        .unwrap_or_default()
        .join(".houyicoder")
        .join("debug.log");
    if let Err(e) = install(&log_path) {
        // Not fatal: the session proceeds without a diagnostic channel, and
        // /debug will report the sink is absent. Logged to stderr here
        // because the alternate screen is not up yet — this is the one
        // window where stderr is a legitimate sink.
        eprintln!("warning: could not install the diagnostic sink: {e}");
    }
    match cmd {
        CliCommand::Help => {
            print_help();
            Ok(())
        }
        #[cfg(unix)]
        CliCommand::Ps => {
            // Reap crashed-session leftovers before listing so ps does not
            // show dead sessions as live.
            let reaped = detach::cleanup_stale();
            if reaped > 0 {
                eprintln!("reaped {reaped} stale detached session(s)");
            }
            for s in detach::list_sessions() {
                let mark = if s.live { "" } else { " (dead)" };
                match s.pid {
                    Some(pid) => println!("{} {}{}", s.id, pid, mark),
                    None => println!("{}{}", s.id, mark),
                }
            }
            Ok(())
        }
        #[cfg(unix)]
        CliCommand::Attach { socket, session } => run_attach(socket, session),
        #[cfg(unix)]
        CliCommand::Serve {
            project,
            socket,
            model,
        } => {
            run_serve(project, socket, model)?;
            Ok(())
        }
        CliCommand::Acp { project, model } => {
            run_acp_stdio(project, model)?;
            Ok(())
        }
        CliCommand::Tui { project, model } => {
            let provider = resolve_provider(project.as_deref());
            let (bundle, startup, host, runner) =
                build_bundle(project.clone(), model, provider.clone());
            let (bundle, startup, lease) =
                acquire_fresh_lease(bundle, startup, host, Some(runner))?;
            run_tui_loop(bundle, startup, lease, project, provider)
        }
        CliCommand::Resume {
            value,
            project,
            fork,
        } => run_resume(value, project, fork),
        CliCommand::Continue { project, fork } => run_continue(project, fork),
        CliCommand::Cleanup {
            apply,
            verbose,
            yes,
        } => run_cleanup(apply, verbose, yes),
    }
}

/// Dispatch --resume <value>. The value is an existing file path (resume from
/// an exported transcript) or a session id (resume a session on disk). --fork-
/// session (fork=true) creates a new sid seeded from the source instead of
/// continuing the source itself.
fn run_resume(value: String, project: Option<String>, fork: bool) -> Result<(), Box<dyn Error>> {
    // Reap crashed-session pid markers before resolving the value, so a sid
    // whose process died is not falsely treated as live (the hash chain
    // single-writer check in is_session_live reads the pid marker).
    #[cfg(unix)]
    {
        let _ = detach::cleanup_stale();
    }
    let provider = resolve_provider(project.as_deref());
    let path = PathBuf::from(&value);
    if path.exists() {
        // Resume-from-export already forks a fresh sid (unique); --fork-session
        // is redundant on this branch.
        let (bundle, startup, host, runner) =
            build_bundle_for_resume(&path, project.clone(), provider.clone())?;
        let (bundle, startup, lease) = acquire_fresh_lease(bundle, startup, host, Some(runner))?;
        run_tui_loop(bundle, startup, lease, project, provider)
    } else if let Some(sid) = houyicoder_context::SessionId::from_display_string(&value) {
        let (bundle, startup, lease) = if fork {
            let (bundle, startup, host, runner) =
                build_bundle_for_fork(sid, project.clone(), provider.clone())?;
            acquire_fresh_lease(bundle, startup, host, Some(runner))?
        } else {
            build_lease_for_existing_sid(sid, project.clone(), provider.clone())?
        };
        run_tui_loop(bundle, startup, lease, project, provider)
    } else {
        eprintln!(
            "resume target is neither an existing file nor a session id: {value} \
             (pass an export file path or a session id: --resume <file.json|sid>)"
        );
        exit(2);
    }
}

/// --continue: resolve the most-recently-active session on disk and resume
/// it (no sid needed). --fork-session creates a new sid seeded from that
/// session instead of continuing it. Errors if no session exists on disk.
fn run_continue(project: Option<String>, fork: bool) -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    {
        let _ = detach::cleanup_stale();
    }
    let sessions_root = session_log_root();
    let Some(sid) = latest_session_sid(&sessions_root) else {
        let cwd = workspace_cwd(None);
        eprintln!(
            "no session in {cwd} to continue\n  --resume <sid>   continue a session by id\n  houyi            start a fresh one"
        );
        exit(2);
    };
    let provider = resolve_provider(project.as_deref());
    let (bundle, startup, lease) = if fork {
        let (bundle, startup, host, runner) =
            build_bundle_for_fork(sid, project.clone(), provider.clone())?;
        acquire_fresh_lease(bundle, startup, host, Some(runner))?
    } else {
        build_lease_for_existing_sid(sid, project.clone(), provider.clone())?
    };
    run_tui_loop(bundle, startup, lease, project, provider)
}

/// Refuse a session id that is live elsewhere or already locked before any
/// writer or host for it is built. Returns the held lock on success. Every
/// existing-sid entry passes through this ahead of its build, so a held
/// session is refused before a writer or a host task is constructed. The live
/// check precedes the lock, so a live detached process gets the friendlier
/// refusal.
fn admit_existing_sid(
    sid: &SessionId,
    sessions_root: &Path,
) -> Result<SessionLock, Box<dyn Error>> {
    let sid_str = sid.to_string();
    #[cfg(unix)]
    if detach::is_session_live(&sid_str) {
        return Err(Box::<dyn Error>::from(format!(
            "session {sid_str} is live in another process; attach to it \
             (houyi attach) or fork (--fork-session)"
        )));
    }
    match SessionLock::acquire(&sid_str, sessions_root) {
        Ok(lock) => Ok(lock),
        // A held lock is the same refusal as a live process: the user can
        // attach or fork, so present that guidance instead of a raw enum.
        Err(LockError::HeldByOther(_)) => Err(Box::<dyn Error>::from(format!(
            "session {sid_str} is held by another live process; attach to it \
             (houyi attach) or fork (--fork-session)"
        ))),
        Err(e) => Err(Box::new(e)),
    }
}

/// Acquire the lock for a freshly-built bundle and return its lease. Called
/// after the build for a new sid (fresh / export / fork), which cannot
/// contend: the sid did not exist before the build created it. If lock
/// acquisition fails, the host tasks and any children already started
/// by the build must be joined and reclaimed so they do not leak.
fn acquire_fresh_lease(
    bundle: RunnerBundle,
    startup: FrontendStartup,
    host: HostLifecycle,
    children: Option<Arc<Runner>>,
) -> Result<(RunnerBundle, FrontendStartup, HostLease), Box<dyn Error>> {
    acquire_fresh_lease_at(bundle, startup, host, children, &session_log_root())
}

fn acquire_fresh_lease_at(
    bundle: RunnerBundle,
    startup: FrontendStartup,
    host: HostLifecycle,
    children: Option<Arc<Runner>>,
    sessions_root: &Path,
) -> Result<(RunnerBundle, FrontendStartup, HostLease), Box<dyn Error>> {
    let sid_str = bundle.session.0.clone();
    let lock = match SessionLock::acquire(&sid_str, sessions_root) {
        Ok(l) => l,
        Err(e) => {
            let runtime = shared_runtime();
            let release = HostLease::unlocked(host, children).begin_release(runtime.handle());
            match runtime.block_on(release) {
                Ok(report) => report.trace_failures("fresh-lease release"),
                Err(err) => tracing::warn!("fresh-lease release task did not finish: {err}"),
            }
            return Err(Box::new(e));
        }
    };
    let lease = HostLease::holding(host, lock, sid_str, children);
    Ok((bundle, startup, lease))
}

/// Build the lease for an existing session id. Admission (live check + lock)
/// runs before the build, so a held or live session is refused without
/// constructing a writer or starting a host for it.
fn build_lease_for_existing_sid(
    sid: SessionId,
    project: Option<String>,
    provider: ProviderResolution,
) -> Result<(RunnerBundle, FrontendStartup, HostLease), Box<dyn Error>> {
    build_lease_for_existing_sid_at(sid, project, provider, &session_log_root())
}

fn build_lease_for_existing_sid_at(
    sid: SessionId,
    project: Option<String>,
    provider: ProviderResolution,
    sessions_root: &Path,
) -> Result<(RunnerBundle, FrontendStartup, HostLease), Box<dyn Error>> {
    let lock = admit_existing_sid(&sid, sessions_root)?;
    let (bundle, startup, host, runner) = build_bundle_for_resume_sid(sid, project, provider)?;
    let sid_str = bundle.session.0.clone();
    let lease = HostLease::holding(host, lock, sid_str, Some(runner));
    Ok((bundle, startup, lease))
}

/// The runner wiring every binary entry shares: the disk persistence
/// preset plus the default permission rule store. One site so a future
/// entry cannot drift onto a different store configuration by accident.
fn production_runner(project: Option<String>, provider: ProviderResolution) -> BuildRunnerOptions {
    let mut options =
        BuildRunnerOptions::disk(project, Some(Arc::new(FileRuleStore::default_paths())));
    options.provider = Some(provider);
    options
}

/// The detached-session entry. Builds the runner through the composition
/// root, registers it under a fresh session in a SessionHost, and binds a
/// Unix domain socket. With a custom socket path, binds there; with None,
/// binds at the conventional per-user path derived from the session id
/// (so ps and stop can discover and control it) and writes a pidfile.
/// Each connecting client reattaches the same session via serve_session;
/// the session survives a client disconnect. The session id is printed to
/// stderr so an attaching client knows the target.
#[cfg(unix)]
fn run_serve(
    project: Option<String>,
    socket: Option<String>,
    model_override: Option<String>,
) -> Result<(), Box<dyn Error>> {
    let provider = resolve_provider(project.as_deref());
    // Taken before the provider moves into the runner: a reattaching client's
    // status needs what the provider was built from.
    let facts = provider.facts.clone();
    let mut bundle = build_runner(production_runner(project, provider));
    if let Some(m) = &model_override {
        bundle.runner.set_model(m.clone());
    }
    eprintln!("session_id={}", bundle.session);
    eprintln!("pid={}", id());
    let socket_path = match socket {
        Some(p) => {
            eprintln!("uds={p}");
            p
        }
        None => {
            let p = detach::session_socket(&bundle.session.to_string());
            detach::write_pidfile(&bundle.session.to_string());
            eprintln!("uds={}", p.display());
            p.to_string_lossy().into_owned()
        }
    };
    // Remove a stale socket file at the path so a fresh bind does not fail on
    // a previous process that crashed without cleanup.
    remove_file(&socket_path).ok();
    let runtime = Builder::new_multi_thread().enable_all().build()?;
    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()).with_provider_facts(facts));
    let event_sequencer = EventSequencer::new();
    event_sequencer.install_on(&mut bundle.runner);
    let relay_cancel = CancellationToken::new();
    let relay_handle = houyicoder_service::composition::fleet_status_relay::spawn(
        bundle.bus.clone(),
        event_sequencer.clone(),
        runtime.handle().clone(),
        relay_cancel.clone(),
    );
    let gate: Arc<dyn ModeGate> = bundle.gate;
    let session = bundle.session;
    host.insert(
        session,
        Arc::new(bundle.runner),
        event_sequencer,
        gate,
        bundle.append_notify,
    );
    runtime.block_on(async move {
        fire_after_bundle(session);
        if let Err(e) = uds::listen_uds(host, session, &socket_path).await {
            eprintln!("uds listener exited: {e}");
        }
        relay_cancel.cancel();
        if let Some(handle) = relay_handle {
            drop(handle.await);
        }
    });
    Ok(())
}

/// Attach a TUI to a detached session. Connects to the session's listening
/// socket, builds a protocol client over the connection, and runs the TUI
/// against the session id the detached process printed to stderr. The TUI
/// holds no runner; the detached process owns it. Disconnecting the TUI
/// leaves the session alive for a later reattach.
#[cfg(unix)]
fn run_attach(socket: String, session_id: String) -> Result<(), Box<dyn Error>> {
    let transport = UdsTransport::connect(&socket, MAX_MESSAGE_BYTES)?;
    let client = Client::new(Box::new(transport));
    let (tx, rx) = channel::<SessionMessage>();
    let wire_session = houyicoder_protocol::frontend::SessionId(session_id);
    let bundle = RunnerBundle {
        client,
        agent_tx: tx,
        agent_rx: rx,
        session: wire_session,
        model: houyicoder_config::resolve_model(),
        trajectory_log: None,
        export_log: None,
        snapshot: None,
        session_catalog: None,
    };
    let startup = FrontendStartup {
        skip_login: false,
        startup_warnings: Vec::new(),
        history_path: houyicoder_config::config_home().join("history.jsonl"),
    };
    // Attach mode has no session_log/descriptor_store, so the /resume picker
    // cannot open (run_resume reports no store wired); no /resume Command is
    // enqueued.
    drop(run_with_runner(bundle, startup, None)?);
    Ok(())
}

/// The ACP stdio entry. Builds the runner through the same composition root
/// the TUI path uses (real provider config, tools, sandbox, memory), then
/// drives the ACP server over stdin + stdout. The session id is assigned up
/// front and printed to stderr so a stock client prompts against it — the
/// server is single-session-bound at construction, so the id the client sees
/// from session/new must match the one the runner drives. Binding the runner
/// to the id session/new assigns (instead of assigning it here) is a later
/// cut; this first cut keeps the simpler pre-assign path the example set.
fn run_acp_stdio(
    project: Option<String>,
    model_override: Option<String>,
) -> Result<(), Box<dyn Error>> {
    let provider = resolve_provider(project.as_deref());
    let bundle = build_runner(production_runner(project, provider));
    if let Some(m) = &model_override {
        bundle.runner.set_model(m.clone());
    }
    // Print the session id to stderr so a client prompts against the right
    // id; stdout stays clean for the JSON-RPC frame stream.
    eprintln!("session_id={}", bundle.session);
    let session = bundle.session;
    let runner = Arc::new(bundle.runner);
    let adapter = Arc::new(AcpAdapter::new(
        AcpxCapabilities::default(),
        1,
        SessionLeaseStore::new(),
    ));

    // mpsc pair: one direction each way. Large capacity so a short turn's
    // frames buffer without a concurrent drain (the drain runs after serve
    // returns); a streaming production carrier drains concurrently to avoid
    // blocking on a full buffer.
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let (server_tx, client_rx) = mpsc::channel::<String>(256);
    let mut io = AcpIo::new(server_tx, server_rx);

    // Bridge stdin to the inbound channel. A dedicated thread does blocking
    // read_line (tokio stdin has an event-loop blocking pitfall); EOF drops
    // the sender so the server sees a clean close.
    std::thread::spawn(move || {
        let stdin = stdin();
        let mut tx = client_tx;
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if tx.try_send(l + "\n").is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Drive the server on a multi-thread runtime so the real provider (HTTP
    // calls) + the sandbox (subprocess spawns) have a runtime that can handle
    // blocking I/O + child reaping. serve returns when the client closes
    // stdin; the buffered outbound frames drain to stdout after.
    let runtime = Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        fire_after_bundle(session);
        let server = AcpServer::new(adapter, runner, session);
        drop(server.serve(&mut io).await);
        // serve borrows io by reference, so io (which holds the outbound
        // sender) outlives serve. Drop it so the drain below terminates.
        drop(io);
        let mut out_rx = client_rx;
        let stdout = stdout();
        let mut out = stdout.lock();
        while let Some(frame) = out_rx.next().await {
            drop(out.write_all(frame.as_bytes()));
            drop(out.write_all(b"\n"));
            drop(out.flush());
        }
    });
    Ok(())
}

/// The TUI run loop. It starts from a lease the entry already admitted (the
/// lock is held before the loop begins), so it never acquires a lock. The
/// event loop swaps sessions in-process when a resume_builder is wired (the
/// normal path: try_swap_session builds the new bundle + swap_session swaps
/// in place, no restart). The builder reaps the prior host lifecycle owner
/// before installing the new one so the old serve, drain, and relay do not
/// outlive the session that ended: the serve exits when its paired client
/// drops, the drain and relay end on the cancel token the owner holds. The
/// Some(target) return is the rebuild fallback for when no builder was
/// wired. None means a clean quit.
type ResumeFn = Box<dyn Fn(&str) -> Result<(RunnerBundle, FrontendStartup), Box<dyn Error>>>;

/// Build a bundle for a resume target, acquiring the target's exclusive
/// lock before constructing any writer that could write its session dir, and
/// reaping the prior lease only after the build succeeds. The lock travels
/// into the new lease so it outlives the builder call: the prior session's
/// writer stays locked in until its host is reaped, not released the moment
/// the build returns.
///
/// The sid-resume path live-checks the target and locks it before the build,
/// because that sid already exists and another process may hold it. The
/// export path forks a fresh, unique sid inside the build, so its lock is
/// acquired after the build for the lease going forward (the new sid is
/// uncontended the moment it is created). A failed build drops the new lock
/// and leaves the prior lease in the slot, so a failed switch strands the
/// user on the live session rather than a dead one.
fn build_bundle_for_target(
    target: &str,
    project: Option<String>,
    provider: ProviderResolution,
    host_slot: &HostLeaseSlot,
) -> Result<(RunnerBundle, FrontendStartup), Box<dyn Error>> {
    let sessions_root = session_log_root();
    let path = PathBuf::from(target);
    let (bundle, startup, host, runner, lock) = if path.exists() {
        let (bundle, startup, host, runner) = build_bundle_for_resume(&path, project, provider)?;
        let new_sid = bundle.session.0.clone();
        // The build started the serve, drain, relay, and refresh tasks on the
        // shared runtime. If the lock fails (rare for a fresh sid, but a disk
        // error or permissions can), the new host must still close so those
        // tasks do not hold the runner's Arc for the process lifetime. The
        // failure is not necessarily a process exit — the error returns to the
        // TUI and the user stays on the live session — so the failed host's
        // release is tracked in the scope, not fire-and-forgot: exit joins it
        // through release_all along with everything else.
        let lock = match SessionLock::acquire(&new_sid, &sessions_root) {
            Ok(l) => l,
            Err(e) => {
                let runtime = shared_runtime();
                let release =
                    HostLease::unlocked(host, Some(runner)).begin_release(runtime.handle());
                host_slot
                    .lock()
                    .expect("host lifecycle slot must not poison")
                    .track_release(release);
                return Err(Box::new(e));
            }
        };
        (bundle, startup, host, runner, lock)
    } else if let Some(sid) = SessionId::from_display_string(target) {
        let sid_str = sid.to_string();
        // Canonical same-session no-op: the live lease already holds this
        // sid's lock, so acquiring again reports it held by this process
        // itself. Compare through the context type so a different spelling
        // of the same session is still a no-op, not a contention error.
        if let Some(current) = host_slot
            .lock()
            .expect("host lifecycle slot must not poison")
            .current_sid()
            && SessionId::from_display_string(current).is_some_and(|c| c == sid)
        {
            return Err(Box::<dyn Error>::from(format!(
                "already on session {sid_str}"
            )));
        }
        let lock = admit_existing_sid(&sid, &sessions_root)?;
        let (bundle, startup, host, runner) = build_bundle_for_resume_sid(sid, project, provider)?;
        (bundle, startup, host, runner, lock)
    } else {
        return Err(Box::<dyn Error>::from(format!(
            "invalid resume target: {target}"
        )));
    };
    let runtime = shared_runtime();
    let handle = runtime.handle();
    // Install the new lease: the old lease releases into a tracked task whose
    // handle stays in the scope's pending list, so a switch-then-exit joins it
    // instead of abandoning the old writers. The old lock outlives the old
    // writers because begin_release joins them before dropping it; the new
    // lease holds a different sid's lock, so the two coexist briefly without
    // contention.
    let lease = HostLease::holding(host, lock, bundle.session.0.clone(), Some(runner));
    host_slot
        .lock()
        .expect("host lifecycle slot must not poison")
        .install(lease, handle);
    Ok((bundle, startup))
}

fn run_tui_loop(
    bundle: RunnerBundle,
    startup: FrontendStartup,
    lease: HostLease,
    project: Option<String>,
    provider: ProviderResolution,
) -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    {
        let _ = detach::cleanup_stale();
    }
    let host_slot: HostLeaseSlot = Arc::new(Mutex::new(HostLeaseScope::with_current(lease)));
    // The guard's drop releases the scope on every exit path, including a
    // panic unwinding past the loop.
    let _lease_guard = LeaseScopeGuard::new(&host_slot);
    let mut bundle = bundle;
    let mut startup = startup;
    let mut resume_builder: Option<ResumeFn> = {
        let host_slot = Arc::clone(&host_slot);
        let project = project.clone();
        let provider = provider.clone();
        Some(Box::new(move |target: &str| {
            build_bundle_for_target(target, project.clone(), provider.clone(), &host_slot)
        }))
    };
    loop {
        match run_with_runner(bundle, startup, resume_builder.take()) {
            Ok(None) => break Ok(()),
            Ok(Some(new_target)) => {
                match build_bundle_for_target(
                    &new_target,
                    project.clone(),
                    provider.clone(),
                    &host_slot,
                ) {
                    Ok((nb, ns)) => {
                        bundle = nb;
                        startup = ns;
                    }
                    Err(e) => break Err(e),
                }
            }
            Err(e) => break Err(Box::new(e)),
        }
    }
}

#[cfg(test)]
mod tests;
