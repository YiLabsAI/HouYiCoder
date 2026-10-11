//! Composition-root assembly: build the runner and protocol server, pair an
//! in-memory client around one shared Arc<Runner>, and return the wired TUI
//! bundle alongside the host lifecycle owner for the session's background
//! tasks. Sits between the CLI entry dispatch and the TUI run loop so the
//! fresh and resume paths share one wiring and cannot drift.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::channel;

use houyicoder_client::{Client, InProcTransport};
use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_service::composition::host_lifecycle::HostLifecycle;
use houyicoder_service::composition::session_log_root;
use houyicoder_service::server::{FrameCarrier, Server};
use houyicoder_tui::agent_message::SessionMessage;
use houyicoder_tui::composition::{FrontendStartup, RunnerBundle, shared_runtime};

use crate::housekeeping;
use crate::session_catalog;
use crate::session_history;
use crate::session_lock::SessionLock;
use crate::session_log_snapshot;
use crate::trajectory;

/// A wired TUI bundle, the frontend startup policy the TUI applies to its Ui
/// and Input owners, and the host lifecycle owner for the session's background
/// tasks. The fresh and resume paths both produce this so a session switch can
/// reap the prior host before installing the new one. The frontend policy
/// travels alongside the bundle rather than inside it so a Session never
/// carries frontend startup state.
pub(crate) type WiredBundle = (RunnerBundle, FrontendStartup, HostLifecycle);

/// A host lifecycle owner holding the session's exclusive lock. The
/// lock outlives every writer the host owns: releasing the lease joins
/// the host's tasks to completion and drops the lock only after, so a
/// second process cannot acquire while a writer is still flushing. A
/// switch releases the old lease after the new bundle builds, so a
/// failed switch leaves the user on the live session with its lease
/// intact. The sid is the canonical display string the lock was acquired
/// for, so a same-session resume is a no-op before re-acquiring a lock
/// this process already holds.
pub(crate) struct HostLease {
    pub(crate) host: HostLifecycle,
    pub(crate) lock: Option<SessionLock>,
    pub(crate) sid: Option<String>,
}

impl HostLease {
    #[cfg(test)]
    pub(crate) fn new(host: HostLifecycle) -> Self {
        Self {
            host,
            lock: None,
            sid: None,
        }
    }

    /// Move the lock into a release task that joins every host task to
    /// completion before dropping the lock. A writer that ignores the
    /// cancel token and exits only on its own gate keeps the lock held
    /// until it finishes; a second process cannot acquire until the join
    /// completes and the lock drops. The caller observes completion through
    /// the returned handle — dropping the handle does not cancel the task,
    /// but the owner must await it or retain the lease, since aborting the
    /// release task (via the handle) drops the lock early.
    pub(crate) fn begin_release(
        self,
        handle: &tokio::runtime::Handle,
    ) -> tokio::task::JoinHandle<()> {
        let host = self.host;
        let lock = self.lock;
        handle.spawn(async move {
            host.begin_shutdown().await;
            drop(lock);
        })
    }
}

/// The slot a run loop holds one host lease in. A switch takes the old
/// lease out, releases it, and stores the new one under the mutex.
pub(crate) type HostLeaseSlot = Arc<Mutex<Option<HostLease>>>;

/// Take the current lease from the slot and release it on the shared
/// runtime: join the host's writers, then drop the lock. Blocks the caller
/// up to the shutdown budget so a writer finishing its last write keeps
/// the lock until it is done — the process does not drop the lock early or
/// abandon a mid-flush writer. Called from the run loop's exit path on
/// both normal and error returns, which runs on the main thread, not a
/// runtime worker — Handle::block_on panics from inside a worker.
pub(crate) fn release_current_lease(host_slot: &HostLeaseSlot) {
    let Some(lease) = host_slot
        .lock()
        .expect("host lifecycle slot must not poison")
        .take()
    else {
        return;
    };
    let runtime = shared_runtime();
    let handle = runtime.handle();
    let release = lease.begin_release(handle);
    if let Err(e) = handle.block_on(release) {
        tracing::warn!("host lease release task did not finish cleanly: {e}");
    }
}

/// The composition root: build the runner via the service layer, install its
/// event handlers, Arc the runner so the server task and the TUI share one, pair an
/// in-memory server + client around it, spawn the server on the shared
/// runtime, and return the wired bundle for the TUI. The TUI never constructs
/// these itself; it stays a presentation layer over the protocol client.
pub(crate) fn build_bundle(
    project: Option<String>,
    model_override: Option<String>,
    provider: houyicoder_service::composition::ResolvedProvider,
) -> WiredBundle {
    let bundle =
        houyicoder_service::composition::build_runner(crate::production_runner(project, provider));
    // --model overrides the settings-seeded active model for a fresh session
    // (resolution chain: --model flag > settings.json > DEFAULT). A resumed
    // session restores its own model from the descriptor (higher priority), so
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
        bundle.bus,
    )
}

/// Resume a session from an exported transcript file (--resume <file> when
/// the value is an existing path). Seeds the new session's log + trajectory
/// from the export, restores the export's model, then wires the same TUI
/// bundle the fresh path uses. The export is a snapshot, so resume forks a
/// new session id (the source sid is recorded in provenance, not reused).
pub(crate) fn build_bundle_for_resume(
    export_path: &std::path::Path,
    project: Option<String>,
    provider: houyicoder_service::composition::ResolvedProvider,
) -> Result<WiredBundle, Box<dyn std::error::Error>> {
    let resumed = houyicoder_service::composition::build_runner_for_resume_export(
        export_path,
        &session_log_root(),
        project,
        Some(Arc::new(
            houyicoder_permission::FileRuleStore::default_paths(),
        )),
        provider,
    )?;
    Ok(assemble_bundle(
        resumed.assembled.runner,
        resumed.assembled.session,
        resumed.model,
        resumed.assembled.gate,
        resumed.assembled.sandbox_session,
        resumed.assembled.append_notify,
        true,
        resumed.assembled.worktree_controller,
        resumed.assembled.bus,
    ))
}

/// Shared TUI wiring: install the agent event handlers, pair the in-memory
/// server and client, build the trajectory view, the export view, and the
/// session-log snapshot over the SessionLog, and assemble the RunnerBundle
/// the TUI drives. Both the fresh path (build_bundle) and the resume path
/// (build_bundle_for_resume) route through here so the wiring cannot drift
/// between them.
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
    bus: Option<Arc<houyicoder_core::agent::multi_agent::bus_types::AgentBus>>,
) -> WiredBundle {
    // Grab the SessionLog BEFORE the runner moves into the server task — the
    // trajectory view reads it for the /trajectory pane, and the session
    // picker reads other sessions' log heads to derive their titles.
    let session_log = runner.store();
    // The descriptor reader for the session picker (lists sessions + reads each
    // name/cwd/model). Built at the same sid-keyed sessions root the file
    // backend uses.
    let descriptor_store: Arc<dyn houyicoder_context::SessionDescriptorStore> =
        houyicoder_service::composition::disk_descriptor_store();
    // The session-log snapshot loads the durable log into a TranscriptLine
    // snapshot for the search view (read-whole path under the threshold).
    // Shares the same SessionLog as the trajectory and export views (an Arc
    // clone, so each view still takes ownership of its slot).
    // One reader owns the byte windows and the offset index for this session;
    // the transcript and the trajectory project what it returns.
    let history = Arc::new(session_history::SessionHistory::new(
        session_log.clone(),
        session,
    ));
    let snapshot: Option<Arc<dyn houyicoder_tui::transcript::snapshot::TranscriptSnapshot>> =
        Some(Arc::new(
            session_log_snapshot::SessionLogSnapshot::with_history(history.clone()),
        ));
    // One object backs both the /trajectory view + the /export serializer:
    // both read the same durable event stream.
    let trajectory = Arc::new(trajectory::SessionLogTrajectory::with_history(
        history,
        session_log.clone(),
        session,
        model.clone(),
    ));
    let trajectory_log: Option<Arc<dyn houyicoder_tui::view::trajectory_pane::TrajectoryLog>> =
        Some(trajectory.clone());
    let export_log: Option<Arc<dyn houyicoder_tui::view::export_log::ExportLog>> = Some(trajectory);
    let wire_session = houyicoder_protocol::frontend::SessionId(session.to_string());
    let (tx, rx) = channel::<SessionMessage>();
    let (_runner, client, startup_warnings, host) = start_local_server(
        runner,
        session,
        gate,
        sandbox_session,
        append_notify,
        Some(descriptor_store.clone()),
        worktree_controller,
        bus,
    );
    // The server task owns the runner + the permission gate; the TUI holds
    // only the protocol client.
    (
        RunnerBundle {
            client,
            agent_tx: tx,
            agent_rx: rx,
            session: wire_session,
            model,
            trajectory_log,
            export_log,
            snapshot,
            session_catalog: Some(Arc::new(session_catalog::DescriptorSessionCatalog::new(
                session_log,
                session_log_root(),
            ))),
        },
        FrontendStartup {
            skip_login,
            startup_warnings,
            history_path: houyicoder_config::config_home().join("history.jsonl"),
        },
        host,
    )
}

/// Pair an in-memory protocol server and client around a runner. The session
/// event sequencer receives runtime events before the runner is shared, then
/// becomes the sole ordering authority for durable event frames, model
/// deltas, and fleet status. The server is the only carrier writer.
#[expect(clippy::too_many_arguments, reason = "param grouping deliberate")]
pub(crate) fn start_local_server(
    mut runner: Runner,
    session: SessionId,
    gate: Arc<houyicoder_permission::DefaultModeGate>,
    sandbox_session: Option<Arc<dyn houyicoder_api::sandbox::SandboxSession>>,
    append_notify: Arc<tokio::sync::Notify>,
    descriptor_store: Option<Arc<dyn houyicoder_context::SessionDescriptorStore>>,
    worktree_controller: Option<Arc<houyicoder_core::agent::WorktreeController>>,
    bus: Option<Arc<houyicoder_core::agent::multi_agent::bus_types::AgentBus>>,
) -> (Arc<Runner>, Client, Vec<String>, HostLifecycle) {
    let mut host = HostLifecycle::new();
    let (c2s_tx, c2s_rx) = futures::channel::mpsc::channel(16);
    let (s2c_tx, s2c_rx) = futures::channel::mpsc::channel(16);
    let event_sequencer = houyicoder_service::server::EventSequencer::new();
    event_sequencer.install_on(&mut runner);
    // The server shares the bus so a child's permission ask (published while
    // the parent run is parked on the child) reaches the wire-approval flow.
    let server_bus = bus.clone();
    // Fleet status relay: bridge bus child status to AgentStatus wire frames so
    // the TUI footer renders without a direct engine-bus dependency.
    if let Some(handle) = houyicoder_service::composition::fleet_status_relay::spawn(
        bus,
        event_sequencer.clone(),
        shared_runtime().handle().clone(),
        host.cancel_token(),
    ) {
        host.track(handle);
    }
    // Route worktree security notices through the same user-visible destination.
    if let Some(controller) = &worktree_controller
        && let Some(handler) = runner.event_handlers().user_notice_handler()
    {
        controller.set_user_notice_handler(handler);
    }
    // Drain startup warnings synchronously before the runner is shared so the
    // host pushes them as initial transcript system lines — no async-sink race.
    let startup_warnings = runner.drain_startup_warnings();
    let runner = Arc::new(runner);
    // Completion notification injector: when a detached (async) child
    // finishes, enqueue a lower-priority notification into the parent's
    // mid-turn queue so the parent model learns the child finished on its
    // next turn boundary. Shares the same bus as the fleet status relay;
    // no-op without one.
    if let Some(handle) = houyicoder_service::composition::notification_drain::spawn(
        server_bus.clone(),
        Arc::downgrade(&runner),
        shared_runtime().handle().clone(),
        host.cancel_token(),
    ) {
        host.track(handle);
    }
    let server_io = FrameCarrier::new(s2c_tx, c2s_rx);
    // The server takes the composition's gate — the same Arc the GuardedTool
    // wrappers hold — so wire /mode and /rules writes reach the gate that
    // actually guards the tools. The TUI holds no gate handle. The sandbox
    // session is threaded in too so the /permissions Workspace verbs can
    // extend the fence at runtime (the same Arc the tools' exec path holds).
    let gate_dyn: Arc<dyn houyicoder_permission::ModeGate> = gate;
    let mut server =
        Server::new_with_event_sequencer(runner.clone(), session, gate_dyn, event_sequencer)
            .with_append_notify(append_notify)
            .with_bus(server_bus);
    if let Some(s) = sandbox_session {
        server = server.with_session(s);
    }
    // Gate the run loop on the startup workspace-trust prompt: an
    // unacknowledged project path asks the TUI client once (the trust card),
    // persists the answer, then enters the frame loop. None when the cwd
    // cannot be read — no prompt fires.
    if let Ok(cwd) = std::env::current_dir() {
        server = server.with_project_path(cwd);
    }
    // Attach the descriptor so /status renders the identity fields (version /
    // name / cwd / provenance). None on paths without a store (tests).
    if let Some(store) = descriptor_store {
        server = server.with_descriptor_store(store);
    }
    let server = server;
    let runtime = shared_runtime();
    let serve_handle = runtime.spawn(async move {
        let _serve = server.serve(server_io).await;
    });
    host.track(serve_handle);
    // Fire background housekeeping (session prune) on the shared runtime.
    // The 10min delay + spawn_blocking run fire-and-forget; the task dies
    // with the process. A short session that exits before 10min simply
    // does not prune this run.
    runtime.spawn(async move {
        housekeeping::fire_after_bundle(session);
    });
    // Ask the provider which model ids it serves and cache them for catalog
    // existence validation. A no-op for the stub provider; a /v1/models fetch
    // for the real one. Failure is silent — the old cache stays, nothing
    // surfaces to the user. Never eprintln here: the TUI is in alt-screen
    // mode and stderr writes corrupt the rendered surface (the cursor sits in
    // the input box, so the text lands there). Tracked by the host so a
    // session switch aborts an in-flight fetch and releases the runner it
    // holds a strong arc to, instead of letting it outlive the session.
    let refresh_handle = runtime.spawn({
        let runner_for_refresh = runner.clone();
        async move {
            drop(runner_for_refresh.refresh_served_models().await);
        }
    });
    host.track(refresh_handle);
    let transport = InProcTransport::from_halves(c2s_tx, s2c_rx);
    let client = Client::new(Box::new(transport));
    (runner, client, startup_warnings, host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_lock::{LockError, SessionLock};
    use houyicoder_context::SessionId;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use tokio::runtime::Builder;
    use tokio::sync::oneshot::channel;

    fn temp_root() -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = env::temp_dir().join(format!("host-lease-{}-{n}", process::id()));
        fs::create_dir_all(&p).expect("temp root");
        p
    }

    /// The exclusive lock must outlive every tracked task the host owns. A
    /// writer that ignores the cancel token and exits only on its own gate
    /// keeps the lock held until it finishes; a second process cannot
    /// acquire while that writer runs, and can acquire only after the
    /// release task joins the writer and drops the lock.
    #[test]
    fn test_lock_outlives_writer() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let root = temp_root();
            let sid = SessionId::new();
            let sid_str = sid.to_string();
            let lock = SessionLock::acquire(&sid_str, &root).expect("first acquire");

            let mut host = HostLifecycle::new();
            let (gate_tx, gate_rx) = channel::<()>();
            let done = Arc::new(AtomicBool::new(false));
            let done_c = Arc::clone(&done);
            host.track(tokio::spawn(async move {
                let _ = gate_rx.await;
                done_c.store(true, Ordering::SeqCst);
            }));

            let lease = HostLease {
                host,
                lock: Some(lock),
                sid: Some(sid_str.clone()),
            };
            let release = lease.begin_release(runtime.handle());

            match SessionLock::acquire(&sid_str, &root) {
                Err(LockError::HeldByOther(_)) => {}
                Ok(_) => panic!("lock released while writer still running"),
                Err(LockError::Io(e)) => panic!("unexpected io error: {e}"),
            }

            drop(gate_tx);
            release.await.expect("release task");

            let _reacquire = SessionLock::acquire(&sid_str, &root).expect("acquire after release");
            assert!(
                done.load(Ordering::SeqCst),
                "writer finished before lock drop"
            );
            fs::remove_dir_all(&root).ok();
        });
    }
}
