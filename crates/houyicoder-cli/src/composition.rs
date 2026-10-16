//! Composition assembly: build the runner and protocol server, pair an
//! in-memory client around one shared Arc<Runner>, and return the wired TUI
//! bundle alongside the host lifecycle owner for the session's background
//! tasks. Sits between the CLI entry dispatch and the TUI run loop so the
//! fresh and resume paths share one wiring and cannot drift.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::channel;

use houyicoder_client::{Client, InProcTransport};
use houyicoder_config::{config_home, resolve_model};
use houyicoder_context::SessionDescriptorStore;
use houyicoder_core::agent::Runner;
use houyicoder_permission::{FileRuleStore, ModeGate};
use houyicoder_protocol::frontend::SessionId as WireSessionId;
use houyicoder_service::composition::host_lifecycle::HostLifecycle;
use houyicoder_service::composition::{
    AssembledRunner, ProviderResolution, build_runner, build_runner_for_resume_export,
    disk_descriptor_store, fleet_status_relay, notification_drain, session_log_root,
};
use houyicoder_service::server::{EventSequencer, FrameCarrier, Server};
use houyicoder_tui::agent_message::SessionMessage;
use houyicoder_tui::composition::{FrontendStartup, RunnerBundle, shared_runtime};
use houyicoder_tui::transcript::snapshot::TranscriptSnapshot;
use houyicoder_tui::view::export_log::ExportLog;
use houyicoder_tui::view::trajectory_pane::TrajectoryLog;

use crate::session::catalog::DescriptorSessionCatalog;
use crate::session::history::SessionHistory;
use crate::session::housekeeping::fire_after_bundle;
use crate::session::log_snapshot::SessionLogSnapshot;
use crate::trajectory::SessionLogTrajectory;
use std::env::current_dir;

/// A wired TUI bundle, the frontend startup policy the TUI applies to its Ui
/// and Input owners, and the host lifecycle owner for the session's background
/// tasks. The fresh and resume paths both produce this so a session switch can
/// reap the prior host before installing the new one. The frontend policy
/// travels alongside the bundle rather than inside it so a Session never
/// carries frontend startup state.
pub(crate) type WiredBundle = (RunnerBundle, FrontendStartup, HostLifecycle, Arc<Runner>);

/// The composition root: build the runner via the service layer, install its
/// event handlers, Arc the runner so the server task and the TUI share one, pair an
/// in-memory server + client around it, spawn the server on the shared
/// runtime, and return the wired bundle for the TUI. The TUI never constructs
/// these itself; it stays a presentation layer over the protocol client.
pub(crate) fn build_bundle(
    project: Option<String>,
    model_override: Option<String>,
    provider: ProviderResolution,
) -> WiredBundle {
    let bundle = build_runner(crate::production_runner(project, provider));
    // --model overrides the settings-seeded active model for a fresh session
    // (resolution chain: --model flag > settings.json > DEFAULT). A resumed
    // session restores its own model from the descriptor (higher priority), so
    // --model is fresh-only (parse_args rejects --model + --resume).
    if let Some(m) = &model_override {
        bundle.runner.set_model(m.clone());
    }
    let model = model_override.unwrap_or_else(resolve_model);
    assemble_bundle(bundle, model, false)
}

/// Resume a session from an exported transcript file (--resume <file> when
/// the value is an existing path). Seeds the new session's log + trajectory
/// from the export, restores the export's model, then wires the same TUI
/// bundle the fresh path uses. The export is a snapshot, so resume forks a
/// new session id (the source sid is recorded in provenance, not reused).
pub(crate) fn build_bundle_for_resume(
    export_path: &Path,
    project: Option<String>,
    provider: ProviderResolution,
) -> Result<WiredBundle, Box<dyn Error>> {
    let resumed = build_runner_for_resume_export(
        export_path,
        &session_log_root(),
        project,
        Some(Arc::new(FileRuleStore::default_paths())),
        provider,
    )?;
    Ok(assemble_bundle(resumed.assembled, resumed.model, true))
}

/// Shared TUI wiring: install the agent event handlers, pair the in-memory
/// server and client, build the trajectory view, the export view, and the
/// session-log snapshot over the SessionLog, and assemble the RunnerBundle
/// the TUI drives. Both the fresh path (build_bundle) and the resume path
/// (build_bundle_for_resume) route through here so the wiring cannot drift
/// between them.
pub(crate) fn assemble_bundle(
    assembled: AssembledRunner,
    model: String,
    skip_login: bool,
) -> WiredBundle {
    let AssembledRunner {
        runner,
        session,
        gate,
        sandbox_session,
        append_notify,
        worktree_controller,
        bus,
        provider_facts,
    } = assembled;
    // Grab the SessionLog BEFORE the runner moves into the server task — the
    // trajectory view reads it for the /trajectory pane, and the session
    // picker reads other sessions' log heads to derive their titles.
    let session_log = runner.store();
    // The descriptor reader for the session picker (lists sessions + reads each
    // name/cwd/model). Built at the same sid-keyed sessions root the file
    // backend uses.
    let descriptor_store: Arc<dyn SessionDescriptorStore> = disk_descriptor_store();
    // The session-log snapshot loads the durable log into a TranscriptLine
    // snapshot for the search view (read-whole path under the threshold).
    // Shares the same SessionLog as the trajectory and export views (an Arc
    // clone, so each view still takes ownership of its slot).
    // One reader owns the byte windows and the offset index for this session;
    // the transcript and the trajectory project what it returns.
    let history = Arc::new(SessionHistory::new(session_log.clone(), session));
    let snapshot: Option<Arc<dyn TranscriptSnapshot>> =
        Some(Arc::new(SessionLogSnapshot::with_history(history.clone())));
    // One object backs both the /trajectory view + the /export serializer:
    // both read the same durable event stream.
    let trajectory = Arc::new(SessionLogTrajectory::with_history(
        history,
        session_log.clone(),
        session,
        model.clone(),
    ));
    let trajectory_log: Option<Arc<dyn TrajectoryLog>> = Some(trajectory.clone());
    let export_log: Option<Arc<dyn ExportLog>> = Some(trajectory);
    let wire_session = WireSessionId(session.to_string());
    let (tx, rx) = channel::<SessionMessage>();
    let (runner, client, startup_warnings, host) = start_local_server(
        AssembledRunner {
            runner,
            session,
            gate,
            sandbox_session,
            append_notify,
            worktree_controller,
            bus,
            provider_facts,
        },
        Some(descriptor_store.clone()),
        None,
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
            session_catalog: Some(Arc::new(DescriptorSessionCatalog::new(
                session_log,
                session_log_root(),
            ))),
        },
        FrontendStartup {
            skip_login,
            startup_warnings,
            history_path: config_home().join("history.jsonl"),
        },
        host,
        runner,
    )
}

/// Pair an in-memory protocol server and client around a runner. The session
/// event sequencer receives runtime events before the runner is shared, then
/// becomes the sole ordering authority for durable event frames, model
/// deltas, and fleet status. The server is the only carrier writer.
pub(crate) fn start_local_server(
    assembled: AssembledRunner,
    descriptor_store: Option<Arc<dyn SessionDescriptorStore>>,
    settings_path: Option<PathBuf>,
) -> (Arc<Runner>, Client, Vec<String>, HostLifecycle) {
    let AssembledRunner {
        mut runner,
        session,
        gate,
        sandbox_session,
        append_notify,
        worktree_controller,
        bus,
        provider_facts,
    } = assembled;
    let mut host = HostLifecycle::new();
    let (c2s_tx, c2s_rx) = futures::channel::mpsc::channel(16);
    let (s2c_tx, s2c_rx) = futures::channel::mpsc::channel(16);
    let event_sequencer = EventSequencer::new();
    event_sequencer.install_on(&mut runner);
    // The server shares the bus so a child's permission ask (published while
    // the parent run is parked on the child) reaches the wire-approval flow.
    let server_bus = bus.clone();
    // Fleet status relay: bridge bus child status to AgentStatus wire frames so
    // the TUI footer renders without a direct engine-bus dependency.
    if let Some(handle) = fleet_status_relay::spawn(
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
    if let Some(handle) = notification_drain::spawn(
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
    let gate_dyn: Arc<dyn ModeGate> = gate;
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
    if let Ok(cwd) = current_dir() {
        server = server.with_project_path(cwd);
    }
    // What the provider was built from, so /status names the host this session
    // actually contacts instead of re-resolving the environment.
    server = server.with_provider_facts(provider_facts);
    // Attach the descriptor so /status renders the identity fields (version /
    // name / cwd / provenance). None on paths without a store (tests).
    if let Some(store) = descriptor_store {
        server = server.with_descriptor_store(store);
    }
    if let Some(sp) = settings_path {
        server = server.with_settings_path(sp);
    }
    let server = server;
    let runtime = shared_runtime();
    // The serve task is pure serve: it returns when its client drops, and
    // the host's shutdown budget may abort it — so it must not be the owner
    // of child completion. A join placed in its tail would be dropped with
    // the aborted future, detaching the child JoinHandles exactly when the
    // lock is about to release.
    let serve_handle = runtime.spawn(async move {
        let _serve = server.serve(server_io).await;
    });
    host.track(serve_handle);
    // Fire background housekeeping (session prune) on the shared runtime.
    // The 10min delay + spawn_blocking run fire-and-forget; the task dies
    // with the process. A short session that exits before 10min simply
    // does not prune this run.
    runtime.spawn(async move {
        fire_after_bundle(session);
    });
    // Ask the provider which model ids it serves and cache them for catalog
    // existence validation. A no-op for the stub provider; a /v1/models fetch
    // for the real one. Failure is silent — the old cache stays, nothing
    // surfaces to the user. Never eprintln here: the TUI is in alt-screen
    // mode and stderr writes corrupt the rendered surface (the cursor sits in
    // the input box, so the text lands there). Tracked by the host so a
    // session switch aborts a running fetch and releases the runner it
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
mod tests;
