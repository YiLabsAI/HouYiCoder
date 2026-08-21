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

use houyicoder_client::{Client, InProcTransport};
use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_service::server::{Server, ServerIo};

#[cfg(unix)]
mod detach;
mod export_bridge;
mod resume_bundle;
mod session_lister_bridge;
mod session_lock;
mod trajectory_bridge;
mod transcript_snapshot_bridge;

/// Parsed CLI invocation. Each variant maps to one entry path the binary
/// dispatches on. Parsing is pure (no I/O, no process exit) so the full
/// flag/subcommand matrix is unit-testable.
mod cli_args;
pub(crate) use cli_args::{CliCommand, parse_args, print_help};

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
            let bundle = build_bundle(project.clone(), model);
            run_tui_loop(bundle, project)
        }
        CliCommand::Resume {
            value,
            project,
            fork,
        } => run_resume(value, project, fork),
        CliCommand::Continue { project, fork } => run_continue(project, fork),
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
    let path = std::path::PathBuf::from(&value);
    if path.exists() {
        // Resume-from-export already forks a fresh sid (unique); --fork-session
        // is redundant on this branch.
        let bundle = build_bundle_for_resume(&path, project.clone())?;
        run_tui_loop(bundle, project)
    } else if let Some(sid) = houyicoder_context::SessionId::from_display_string(&value) {
        let bundle = if fork {
            resume_bundle::build_bundle_for_fork(sid, project.clone())?
        } else {
            resume_bundle::build_bundle_for_resume_sid(sid, project.clone())?
        };
        run_tui_loop(bundle, project)
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
    let bundle = if fork {
        resume_bundle::build_bundle_for_fork(sid, project.clone())?
    } else {
        resume_bundle::build_bundle_for_resume_sid(sid, project.clone())?
    };
    run_tui_loop(bundle, project)
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
) -> houyicoder_service::composition::BuildRunnerOptions {
    houyicoder_service::composition::BuildRunnerOptions::disk(
        project,
        Some(std::sync::Arc::new(
            houyicoder_permission::FileRuleStore::default_paths(),
        )),
    )
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
    use std::sync::atomic::AtomicU64;

    let bundle = build_runner(production_runner(project));
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
    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    let next_seq = Arc::new(AtomicU64::new(0));
    let gate: Arc<dyn ModeGate> = bundle.gate;
    let session = bundle.session;
    host.insert(session, Arc::new(bundle.runner), next_seq, gate);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        if let Err(e) = uds::listen_uds(host, session, &socket_path).await {
            eprintln!("uds listener exited: {e}");
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
    let (tx, rx) = std::sync::mpsc::channel::<houyicoder_tui::run_control::AgentMessage>();
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
        session_lister: None,
        skip_login: false,
        startup_warnings: Vec::new(),
    };
    // Attach mode has no session_log/meta_store, so the /resume picker cannot
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

    let bundle = houyicoder_service::composition::build_runner(production_runner(project));
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
/// new bundle + swap_session swaps in place, no restart). The Some(target)
/// return is a fallback for when no builder was wired: re-build the bundle
/// for that target + re-enter. The prior run_with_runner returned, so the old
/// App + its client dropped + the old inproc server self-terminated on
/// disconnect — no leak. None means a clean quit.
type ResumeFn = Box<
    dyn Fn(&str) -> Result<houyicoder_tui::composition::RunnerBundle, Box<dyn std::error::Error>>,
>;

fn run_tui_loop(
    mut bundle: houyicoder_tui::composition::RunnerBundle,
    project: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut resume_builder: Option<ResumeFn> = {
        let project = project.clone();
        Some(Box::new(move |target: &str| {
            let path = std::path::PathBuf::from(target);
            if path.exists() {
                build_bundle_for_resume(&path, project.clone())
            } else if let Some(sid) = houyicoder_context::SessionId::from_display_string(target) {
                resume_bundle::build_bundle_for_resume_sid(sid, project.clone())
            } else {
                Err(Box::<dyn std::error::Error>::from(format!(
                    "invalid resume target: {target}"
                )))
            }
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
                // The pending target from the picker or /resume <arg> carries
                // a session id OR an export file path. Dispatch on which: an
                // existing .json path resumes from an exported transcript
                // (forks a fresh sid, provenance ResumedFromExport); a valid
                // sid string resumes a session on disk; anything else is an
                // error. Mirrors the CLI --resume <value> dispatch.
                let path = std::path::PathBuf::from(&new_target);
                if path.exists() {
                    bundle = build_bundle_for_resume(&path, project.clone())?;
                } else if let Some(sid) =
                    houyicoder_context::SessionId::from_display_string(&new_target)
                {
                    bundle = resume_bundle::build_bundle_for_resume_sid(sid, project.clone())?;
                } else {
                    return Err(Box::<dyn std::error::Error>::from(format!(
                        "invalid resume target from picker: {new_target}"
                    )));
                }
            }
        }
    }
}

/// The composition root: build the runner via the service layer, install the
/// live sink (so streamed deltas ship to the TUI while the server's run
/// drives), Arc the runner so the server task and the TUI share one, pair an
/// in-memory server + client around it, spawn the server on the shared
/// runtime, and return the wired bundle for the TUI. The TUI never constructs
/// these itself; it stays a presentation layer over the protocol client.
fn build_bundle(
    project: Option<String>,
    model_override: Option<String>,
) -> houyicoder_tui::composition::RunnerBundle {
    let bundle = houyicoder_service::composition::build_runner(production_runner(project));
    // --model overrides the settings-seeded active model for a fresh session
    // (resolution chain: --model flag > settings.json > DEFAULT). A resumed
    // session restores its own model from the sidecar (higher priority), so
    // --model is fresh-only (parse_args rejects --model + --resume).
    if let Some(m) = &model_override {
        bundle.runner.set_model(m.clone());
    }
    let model = model_override.unwrap_or_else(houyicoder_config::resolve_model);
    assemble_bundle(
        bundle.runner,
        bundle.session,
        model,
        bundle.gate,
        bundle.sandbox_session,
        bundle.append_notify,
        false,
        bundle.worktree_controller,
    )
}

/// Resume a session from an exported transcript file (--resume <file> when
/// the value is an existing path). Seeds the new session's log + trajectory
/// from the export, restores the export's model, then wires the same TUI
/// bundle the fresh path uses. The export is a snapshot, so resume forks a
/// new session id (the source sid is recorded in provenance, not reused).
fn build_bundle_for_resume(
    export_path: &std::path::Path,
    project: Option<String>,
) -> Result<houyicoder_tui::composition::RunnerBundle, Box<dyn std::error::Error>> {
    let resumed = houyicoder_service::composition::build_runner_for_resume_export(
        export_path,
        &houyicoder_service::composition::session_log_root(),
        project,
        Some(Arc::new(
            houyicoder_permission::FileRuleStore::default_paths(),
        )),
    )
    .map_err(|e| {
        eprintln!("resume failed: {e}");
        e
    })?;
    Ok(assemble_bundle(
        resumed.assembled.runner,
        resumed.assembled.session,
        resumed.model,
        resumed.assembled.gate,
        resumed.assembled.sandbox_session,
        resumed.assembled.append_notify,
        true,
        resumed.assembled.worktree_controller,
    ))
}

/// Shared TUI wiring: install the live delta sink, pair the in-memory
/// server and client, build the trajectory, export, and snapshot bridges
/// over the SessionLog, and assemble the RunnerBundle the TUI drives. Both
/// the fresh path (build_bundle) and the resume path
/// (build_bundle_for_resume) route through here so the bridge wiring
/// cannot drift between them.
#[expect(clippy::too_many_arguments, reason = "param grouping deliberate")]
pub(crate) fn assemble_bundle(
    runner: Runner,
    session: SessionId,
    model: String,
    gate: Arc<houyicoder_permission::DefaultModeGate>,
    sandbox_session: Option<Arc<dyn houyicoder_api::sandbox::SandboxSession>>,
    append_notify: Arc<tokio::sync::Notify>,
    skip_login: bool,
    worktree_controller: Option<Arc<houyicoder_core::agent::WorktreeController>>,
) -> houyicoder_tui::composition::RunnerBundle {
    // Grab the SessionLog BEFORE the runner moves into the server task — the
    // trajectory bridge projects it for the /trajectory pane, and the session
    // picker reads other sessions' log heads to derive their titles.
    let session_log = runner.store();
    let bridge_session_id = session;
    // The sidecar reader for the session picker (lists sessions + reads each
    // name/cwd/model). Built at the same sid-keyed sessions root the file
    // backend uses.
    let meta_store: std::sync::Arc<dyn houyicoder_context::SessionMetaStore> =
        houyicoder_service::composition::disk_meta_store();
    // The snapshot bridge loads the durable log into a TranscriptLine
    // snapshot for the search view (read-whole path under the threshold).
    // Shares the same SessionLog as the trajectory/export bridges (an Arc
    // clone, so the trajectory bridge still takes ownership of its slot).
    let snapshot: Option<
        std::sync::Arc<dyn houyicoder_tui::transcript::snapshot::TranscriptSnapshot>,
    > = Some(std::sync::Arc::new(
        transcript_snapshot_bridge::SessionLogSnapshot::new(session_log.clone(), bridge_session_id),
    ));
    // One bridge object backs both the /trajectory view + the /export
    // serializer — both project the same durable event stream, so a single
    // Arc<SessionLogTrajectory> is coerced to each trait object the TUI holds.
    let trajectory = std::sync::Arc::new(trajectory_bridge::SessionLogTrajectory::new(
        session_log.clone(),
        bridge_session_id,
        model.clone(),
    ));
    let trajectory_log: Option<
        std::sync::Arc<dyn houyicoder_tui::view::trajectory_pane::TrajectoryLog>,
    > = Some(trajectory.clone());
    let export_log: Option<std::sync::Arc<dyn houyicoder_tui::view::export_log::ExportLog>> =
        Some(trajectory);
    let wire_session = houyicoder_protocol::frontend::SessionId(session.to_string());
    let (tx, rx) = std::sync::mpsc::channel::<houyicoder_tui::run_control::AgentMessage>();
    let (_runner, client, startup_warnings) = pair_inproc_server(
        runner,
        session,
        gate,
        sandbox_session,
        append_notify,
        Some(meta_store.clone()),
        worktree_controller,
    );
    // The server task owns the runner + the permission gate; the TUI holds
    // only the protocol client.
    houyicoder_tui::composition::RunnerBundle {
        client,
        agent_tx: tx,
        agent_rx: rx,
        session: wire_session,
        model,
        trajectory_log,
        export_log,
        snapshot,
        session_lister: Some(std::sync::Arc::new(
            session_lister_bridge::SessionListerBridge::new(
                meta_store,
                session_log,
                houyicoder_service::composition::session_log_root(),
            ),
        )),
        skip_login,
        startup_warnings,
    }
}

/// Pair an in-memory protocol server + client around a runner. Installs the
/// live delta sink before Arc-ing (set_live_sink takes &mut self): the sink
/// streams token-level deltas onto the wire as acpx/llm/* notifications during
/// the server's run, so streaming rides the wire (not a shared runner handle)
/// and the TUI never imports the ports live types. The shared event-seq
/// counter is created here and passed to both the sink and the server so live
/// deltas and durable turn events share one monotonic seq stream. Both ends
/// share one futures mpsc channel pair. The server is spawned on the shared
/// runtime the TUI owns; the client is returned un-connected (the TUI driver
/// task performs the Hello handshake on spawn).
fn pair_inproc_server(
    mut runner: Runner,
    session: SessionId,
    gate: Arc<houyicoder_permission::DefaultModeGate>,
    sandbox_session: Option<Arc<dyn houyicoder_api::sandbox::SandboxSession>>,
    append_notify: Arc<tokio::sync::Notify>,
    meta_store: Option<Arc<dyn houyicoder_context::SessionMetaStore>>,
    worktree_controller: Option<Arc<houyicoder_core::agent::WorktreeController>>,
) -> (Arc<Runner>, Client, Vec<String>) {
    let (c2s_tx, c2s_rx) = futures::channel::mpsc::channel(16);
    let (s2c_tx, s2c_rx) = futures::channel::mpsc::channel(16);
    let next_seq = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    houyicoder_service::server::install_delta_sink(&mut runner, s2c_tx.clone(), next_seq.clone());
    // Share the runner's live sink with the worktree controller so the
    // main-branch-moved alert surfaces as a system line the user sees, not
    // just a diagnostic log entry. The controller was built before the
    // runner (staged delegation); attach the sink now that it exists.
    if let Some(controller) = &worktree_controller
        && let Some(sink) = runner.live_sink()
    {
        controller.set_live_sink(sink);
    }
    // Drain startup warnings synchronously before the runner is shared so the
    // host pushes them as initial transcript system lines — no async-sink race.
    let startup_warnings = runner.drain_startup_warnings();
    let runner = Arc::new(runner);
    let server_io = ServerIo::new(s2c_tx, c2s_rx);
    // The server takes the composition's gate — the same Arc the GuardedTool
    // wrappers hold — so wire /mode and /rules writes reach the gate that
    // actually guards the tools. The TUI holds no gate handle. The sandbox
    // session is threaded in too so the /permissions Workspace verbs can
    // extend the fence at runtime (the same Arc the tools' exec path holds).
    let gate_dyn: Arc<dyn houyicoder_permission::ModeGate> = gate;
    let mut server = Server::new_with_shared_seq(runner.clone(), session, gate_dyn, next_seq)
        .with_append_notify(append_notify);
    if let Some(s) = sandbox_session {
        server = server.with_session(s);
    }
    // Attach the sidecar so /status renders the identity fields (version /
    // name / cwd / provenance). None on paths without a store (tests).
    if let Some(store) = meta_store {
        server = server.with_meta_store(store);
    }
    let server = server;
    let runtime = houyicoder_tui::composition::shared_runtime();
    runtime.spawn(async move {
        let _serve = server.serve(server_io).await;
    });
    // Fire-and-forget: ask the provider which model ids it serves and cache
    // them for catalog existence validation. A no-op for the stub provider;
    // a /v1/models fetch for the real one. Failure is silent — the old
    // cache stays, nothing surfaces to the user. Never eprintln here: the
    // TUI is in alt-screen mode and stderr writes corrupt the rendered
    // surface (the cursor sits in the input box, so the text lands there).
    runtime.spawn({
        let runner_for_refresh = runner.clone();
        async move {
            drop(runner_for_refresh.refresh_served_models().await);
        }
    });
    let transport = InProcTransport::from_halves(c2s_tx, s2c_rx);
    let client = Client::new(Box::new(transport));
    (runner, client, startup_warnings)
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
