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

use std::sync::Arc;
use std::sync::mpsc::channel;

use houyicoder_tui::agent_message::SessionMessage;

mod cleanup;
mod composition_root;
#[cfg(unix)]
mod detach;
mod export_view;
mod housekeeping;
mod resume_bundle;
mod session_catalog;
mod session_history;
mod session_lock;
mod session_log_snapshot;
mod trajectory;

/// Parsed CLI invocation. Each variant maps to one entry path the binary
/// dispatches on. Parsing is pure (no I/O, no process exit) so the full
/// flag/subcommand matrix is unit-testable.
mod cli_args;
pub(crate) use cli_args::{CliCommand, parse_args, print_help};

pub(crate) use composition_root::{WiredBundle, assemble_bundle};
use composition_root::{build_bundle, build_bundle_for_resume};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match parse_args(args) {
        Ok(cmd) => cmd,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
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
    let log_path = std::env::current_dir()
        .unwrap_or_default()
        .join(".houyicoder")
        .join("debug.log");
    if let Err(e) = houyicoder_service::diagnostics::install(&log_path) {
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
            let provider = houyicoder_service::composition::resolve_provider(project.as_deref());
            let (bundle, host) = build_bundle(project.clone(), model, provider.clone());
            run_tui_loop(bundle, host, project, provider)
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
        } => cleanup::run_cleanup(apply, verbose, yes),
    }
}

/// Dispatch --resume <value>. The value is an existing file path (resume from
/// an exported transcript) or a session id (resume a session on disk). --fork-
/// session (fork=true) mints a new sid seeded from the source instead of
/// continuing the source itself.
fn run_resume(
    value: String,
    project: Option<String>,
    fork: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Reap crashed-session pid markers before resolving the value, so a sid
    // whose process died is not falsely treated as live (the hash chain
    // single-writer check in is_session_live reads the pid marker).
    #[cfg(unix)]
    {
        let _ = detach::cleanup_stale();
    }
    let provider = houyicoder_service::composition::resolve_provider(project.as_deref());
    let path = std::path::PathBuf::from(&value);
    if path.exists() {
        // Resume-from-export already forks a fresh sid (unique); --fork-session
        // is redundant on this branch.
        let (bundle, host) = build_bundle_for_resume(&path, project.clone(), provider.clone())?;
        run_tui_loop(bundle, host, project, provider)
    } else if let Some(sid) = houyicoder_context::SessionId::from_display_string(&value) {
        let (bundle, host) = if fork {
            resume_bundle::build_bundle_for_fork(sid, project.clone(), provider.clone())?
        } else {
            resume_bundle::build_bundle_for_resume_sid(sid, project.clone(), provider.clone())?
        };
        run_tui_loop(bundle, host, project, provider)
    } else {
        eprintln!(
            "resume target is neither an existing file nor a session id: {value} \
             (pass an export file path or a session id: --resume <file.json|sid>)"
        );
        std::process::exit(2);
    }
}

/// --continue: resolve the most-recently-active session on disk and resume
/// it (no sid needed). --fork-session mints a new sid seeded from that
/// session instead of continuing it. Errors if no session exists on disk.
fn run_continue(project: Option<String>, fork: bool) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        let _ = detach::cleanup_stale();
    }
    let sessions_root = houyicoder_service::composition::session_log_root();
    let Some(sid) = houyicoder_service::composition::latest_session_sid(&sessions_root) else {
        let cwd = houyicoder_service::composition::workspace_cwd(None);
        eprintln!(
            "no session in {cwd} to continue\n  --resume <sid>   continue a session by id\n  houyi            start a fresh one"
        );
        std::process::exit(2);
    };
    let provider = houyicoder_service::composition::resolve_provider(project.as_deref());
    let (bundle, host) = if fork {
        resume_bundle::build_bundle_for_fork(sid, project.clone(), provider.clone())?
    } else {
        resume_bundle::build_bundle_for_resume_sid(sid, project.clone(), provider.clone())?
    };
    run_tui_loop(bundle, host, project, provider)
}

/// Acquire the per-session exclusive lock, held for the caller's process
/// lifetime (the guard's Drop releases it). Unix-only; a non-unix build has
/// no flock, so the lock is a no-op (returns Ok with no guard held).
#[cfg(unix)]
fn acquire_resume_lock(
    sid_str: &str,
) -> Result<session_lock::SessionLock, Box<dyn std::error::Error>> {
    Ok(session_lock::SessionLock::acquire(
        sid_str,
        &houyicoder_service::composition::session_log_root(),
    )
    .map_err(|e| {
        eprintln!("resume failed: {e}; fork a new session instead (--fork-session, coming)");
        Box::new(e)
    })?)
}

#[cfg(not(unix))]
fn acquire_resume_lock(_sid_str: &str) -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

/// The runner wiring every binary entry shares: the disk persistence
/// preset plus the default permission rule store. One site so a future
/// entry cannot drift onto a different store configuration by accident.
fn production_runner(
    project: Option<String>,
    provider: houyicoder_service::composition::ResolvedProvider,
) -> houyicoder_service::composition::BuildRunnerOptions {
    let mut options = houyicoder_service::composition::BuildRunnerOptions::disk(
        project,
        Some(std::sync::Arc::new(
            houyicoder_permission::FileRuleStore::default_paths(),
        )),
    );
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
) -> Result<(), Box<dyn std::error::Error>> {
    use houyicoder_permission::ModeGate;
    use houyicoder_service::composition::{SessionHost, build_runner};
    use houyicoder_service::lifecycle::SessionLeaseStore;
    use houyicoder_service::uds;
    use std::sync::Arc;

    let provider = houyicoder_service::composition::resolve_provider(project.as_deref());
    let mut bundle = build_runner(production_runner(project, provider));
    if let Some(m) = &model_override {
        bundle.runner.set_model(m.clone());
    }
    eprintln!("session_id={}", bundle.session);
    eprintln!("pid={}", std::process::id());
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
    std::fs::remove_file(&socket_path).ok();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    let event_sequencer = houyicoder_service::server::EventSequencer::new();
    event_sequencer.install_on(&mut bundle.runner);
    let relay_cancel = houyicoder_service::CancellationToken::new();
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
        housekeeping::fire_after_bundle(session);
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
fn run_attach(socket: String, session_id: String) -> Result<(), Box<dyn std::error::Error>> {
    use houyicoder_client::{Client, UdsTransport};
    let transport = UdsTransport::connect(&socket, 1024 * 1024)?;
    let client = Client::new(Box::new(transport));
    let (tx, rx) = channel::<SessionMessage>();
    let wire_session = houyicoder_protocol::frontend::SessionId(session_id);
    let bundle = houyicoder_tui::composition::RunnerBundle {
        client,
        agent_tx: tx,
        agent_rx: rx,
        session: wire_session,
        model: houyicoder_config::resolve_model(),
        trajectory_log: None,
        export_log: None,
        snapshot: None,
        session_catalog: None,
        skip_login: false,
        startup_warnings: Vec::new(),
        history_path: houyicoder_config::config_home().join("history.jsonl"),
    };
    // Attach mode has no session_log/descriptor_store, so the /resume picker cannot
    // open (run_resume reports no store wired); pending_resume_target stays None.
    drop(houyicoder_tui::app::run_with_runner(bundle, None)?);
    Ok(())
}

/// The ACP stdio entry. Builds the runner through the same composition root
/// the TUI path uses (real provider config, tools, sandbox, memory), then
/// drives the ACP server over stdin + stdout. The session id is pre-minted
/// and printed to stderr so a stock client prompts against it — the server
/// is single-session-bound at construction, so the id the client sees from
/// session/new must match the one the runner drives. Binding the runner to
/// the id session/new mints (instead of pre-minting here) is a later cut;
/// this first cut keeps the simpler pre-mint path the example established.
fn run_acp_stdio(
    project: Option<String>,
    model_override: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    use futures::StreamExt;
    use futures::channel::mpsc;
    use houyicoder_protocol::acpx::AcpxCapabilities;
    use houyicoder_service::acp_adapter::AcpAdapter;
    use houyicoder_service::acp_serve::AcpIo;
    use houyicoder_service::acp_server::AcpServer;
    use houyicoder_service::lifecycle::SessionLeaseStore;
    use std::io::{BufRead, Write};

    let provider = houyicoder_service::composition::resolve_provider(project.as_deref());
    let bundle =
        houyicoder_service::composition::build_runner(production_runner(project, provider));
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
        let stdin = std::io::stdin();
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
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        housekeeping::fire_after_bundle(session);
        let server = AcpServer::new(adapter, runner, session);
        drop(server.serve(&mut io).await);
        // serve borrows io by reference, so io (which holds the outbound
        // sender) outlives serve. Drop it so the drain below terminates.
        drop(io);
        let mut out_rx = client_rx;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        while let Some(frame) = out_rx.next().await {
            drop(out.write_all(frame.as_bytes()));
            drop(out.write_all(b"\n"));
            drop(out.flush());
        }
    });
    Ok(())
}

/// The TUI run loop. Before each run_with_runner call, acquire the bundle
/// session's lock (single-writer guard) + refuse if a live --serve process
/// holds it (is_session_live). The event loop swaps sessions in-process when
/// a resume_builder is wired (the normal path: try_swap_session builds the
/// new bundle + swap_session swaps in place, no restart). The builder reaps
/// the prior host lifecycle owner before installing the new one so the old
/// serve, drain, and relay do not outlive the session that ended: the serve
/// exits when its paired client drops, the drain and relay end on the cancel
/// token the owner holds. The Some(target) return is the rebuild fallback
/// for when no builder was wired. None means a clean quit.
type ResumeFn = Box<
    dyn Fn(&str) -> Result<houyicoder_tui::composition::RunnerBundle, Box<dyn std::error::Error>>,
>;

/// Build a bundle for a resume target, reaping the prior host only after the
/// build succeeds. Shared by the in-process resume closure and the rebuild
/// fallback so neither path leaks the prior serve, drain, and relay tasks or
/// the Runner they hold. Reaping after the build keeps the prior session's
/// tasks alive when the target is invalid or the build fails, so a failed
/// switch strands the user on the live session rather than a dead one.
fn build_bundle_for_target(
    target: &str,
    project: Option<String>,
    provider: houyicoder_service::composition::ResolvedProvider,
    host_slot: &houyicoder_service::composition::host_lifecycle::HostSlot,
) -> Result<houyicoder_tui::composition::RunnerBundle, Box<dyn std::error::Error>> {
    let path = std::path::PathBuf::from(target);
    let (bundle, host) = if path.exists() {
        build_bundle_for_resume(&path, project, provider)?
    } else if let Some(sid) = houyicoder_context::SessionId::from_display_string(target) {
        resume_bundle::build_bundle_for_resume_sid(sid, project, provider)?
    } else {
        return Err(Box::<dyn std::error::Error>::from(format!(
            "invalid resume target: {target}"
        )));
    };
    let runtime = houyicoder_tui::composition::shared_runtime();
    if let Some(old) = host_slot
        .lock()
        .expect("host lifecycle slot must not poison")
        .take()
    {
        old.shutdown(runtime.handle());
    }
    *host_slot
        .lock()
        .expect("host lifecycle slot must not poison") = Some(host);
    Ok(bundle)
}

fn run_tui_loop(
    bundle: houyicoder_tui::composition::RunnerBundle,
    host: houyicoder_service::composition::host_lifecycle::HostLifecycle,
    project: Option<String>,
    provider: houyicoder_service::composition::ResolvedProvider,
) -> Result<(), Box<dyn std::error::Error>> {
    let host_slot: houyicoder_service::composition::host_lifecycle::HostSlot =
        std::sync::Arc::new(std::sync::Mutex::new(Some(host)));
    let mut bundle = bundle;
    let mut resume_builder: Option<ResumeFn> = {
        let host_slot = std::sync::Arc::clone(&host_slot);
        let project = project.clone();
        let provider = provider.clone();
        Some(Box::new(move |target: &str| {
            build_bundle_for_target(target, project.clone(), provider.clone(), &host_slot)
        }))
    };
    loop {
        let sid_str = bundle.session.0.clone();
        #[cfg(unix)]
        {
            let _ = detach::cleanup_stale();
            if detach::is_session_live(&sid_str) {
                eprintln!(
                    "session {sid_str} is live in another process; \
                     attach to it (houyi attach) or fork (--fork-session, coming)"
                );
                std::process::exit(2);
            }
        }
        let _lock = acquire_resume_lock(&sid_str)?;
        match houyicoder_tui::app::run_with_runner(bundle, resume_builder.take())? {
            None => return Ok(()),
            Some(new_target) => {
                bundle = build_bundle_for_target(
                    &new_target,
                    project.clone(),
                    provider.clone(),
                    &host_slot,
                )?;
            }
        }
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
