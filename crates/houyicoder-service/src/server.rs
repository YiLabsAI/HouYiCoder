//! Protocol server — the service role of the protocol connection (not a
//! standalone process; the process entry is the cli composition root, which
//! assembles the runner + this server + the client). Performs the Hello
//! handshake, routes incoming requests to the runner, and pushes events back
//! to the client on the monotonic event stream. Holds the resume cursor so a
//! reconnecting client reports the last seq it processed and the server
//! replays the tail without re-sending the whole log.
//!
//! Layering (adjudicated 2026-07-16): the carrier abstraction the frontend
//! holds is a client-side concern. The service server reads raw frame I/O
//! directly — a futures mpsc channel pair for the in-memory carrier here, a
//! framed byte reader for pipes later. The shared contract is the protocol
//! frame format (NDJSON frames of protocol serde types), never the client
//! carrier trait, so the service never depends on the layer above it.

#![allow(dead_code)] // pub server type consumed by other crates and tests; locally unused

use std::sync::Arc;

use crate::composition::multi_agent::permission_request_from;
use houyicoder_async::bus::MessageBus;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, permission_request_topic,
};

use tokio::sync::Notify;
use tokio::sync::broadcast::Receiver;

use crate::composition::SessionHost;
use crate::composition::background_tasks::BackgroundTasks;
use crate::protocol_adapter::{map_run_error, map_run_result};
use houyicoder_provider::ProviderFacts;

use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_core::snapshot::file_transaction::FileTransaction;
use houyicoder_protocol::envelope::{ClientFrame, EventSeq, RequestId, ResponsePayload};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::handshake::{Hello, Negotiated, negotiate};

mod event_sequencer;
pub use event_sequencer::EventSequencer;

/// The connection's raw frame I/O carrier.
mod frame_carrier;
use crate::diagnostics::DiagnosticsHandle;
use crate::diagnostics::handle;
use crate::lifecycle::PendingPermission;
use crate::lifecycle::PendingTurn;
pub use frame_carrier::FrameCarrier;
use futures::future::Either;
use futures::future::pending;
use houyicoder_api::agent_event::UserNoticeEvent;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_core::agent::{
    ApprovalDecision as EngineApprovalDecision, ApprovalRequest as EngineApprovalRequest, RunError,
    RunOutcome, RunResult,
};
use houyicoder_protocol::acp_wire::AcpNotification;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use tokio::time::Instant;
use tokio::time::sleep_until;

use driving::OnClose;

use crate::server::recap::RecapAttachment;
use crate::server::recap::RecapSession;
use crate::server::recap::job::Outcome;
use crate::server::recap::job::run as run_recap;

/// Request routing plus the per-domain request and notification handlers.
mod capability_handler;
mod checkpoint_handler;
mod context_handler;
mod memory_handler;
mod notification_handler;
mod permission_handler;
mod request_router;
mod status_handler;
mod worktree_handler;

/// The /debug request handler.
mod debug_dispatch;

/// Status-snapshot descriptor attachers: env/config display fields + the
/// per-model usage view.
mod status_view;

/// The one place a model pick is applied: the between-runs dispatch and the
/// mid-run request handler both route through it, so a switch takes the same
/// path whether or not a turn is in flight.
mod model_apply;

/// The mid-run permission reverse-request: ask the human, record the verdict
/// audit, apply consent.
mod approval;
mod trust;

/// The wait loop that keeps a running host operation answered.
mod driving;

/// The notification-consumption turn an idle client asked for.
mod notification_turn;

/// Outbound frame emission: push durable events, send typed responses/events
/// on the seq stream.
mod outbound_frames;

/// The protocol server. Owns the runner handle + the session the connection
/// drives, plus the monotonic event seq counter. serve runs the handshake
/// then the request loop until the client disconnects or a fatal frame error
/// surfaces.
enum ServeWake {
    Frame(Result<Option<String>, ProtocolError>),
    Event,
    Recap,
    /// A background child's permission ask arrived between runs.
    Permission(BusMessage),
}

pub struct Server {
    runner: Arc<Runner>,
    session: SessionId,
    event_sequencer: EventSequencer,
    replay_after: Option<EventSeq>,
    next_req_id: u64,
    gate: Arc<dyn houyicoder_permission::ModeGate>,
    sandbox_session: Option<Arc<dyn SandboxSession>>,
    file_transaction: Option<Arc<FileTransaction>>,
    host: Option<Arc<SessionHost>>,
    settings_path: PathBuf,
    project_path: Option<PathBuf>,
    provider_facts: Option<ProviderFacts>,
    append_notify: Option<Arc<Notify>>,
    descriptor_store: Option<Arc<dyn houyicoder_context::SessionDescriptorStore>>,
    diagnostics: Option<DiagnosticsHandle>,
    bus: Option<Arc<AgentBus>>,
    /// The session's recap state. One instance per session, shared across
    /// connections; None on a server assembled without the feature, which
    /// never starts a request.
    recap: Option<Arc<RecapSession>>,
    /// The session's background command registry, so the tasks list, the
    /// detail view, and the kill request read the same instance the shell
    /// tool passes runs to. None on a server built without one, which reports
    /// an empty list.
    background_tasks: Option<Arc<BackgroundTasks>>,
    /// This connection's registration with the recap state. Dropping it
    /// detaches the client, so a closed connection cannot keep a summary
    /// waiting for nobody.
    recap_client: Option<RecapAttachment>,
}

pub(crate) mod child_permission;
/// The away and resume recap: when one may run, and the request it makes.
/// Public because a server is built with the recap state the session was
/// assembled with, so an embedder that builds a server builds this too.
pub mod recap;
/// Reconnect-replay entry points: serve_session, resume_pending.
pub(crate) mod session;

impl Server {
    /// Build a server with a fresh session event sequencer.
    pub fn new(
        runner: Arc<Runner>,
        session: SessionId,
        gate: Arc<dyn houyicoder_permission::ModeGate>,
    ) -> Self {
        Self::new_with_event_sequencer(runner, session, gate, EventSequencer::new())
    }

    /// Build a server bound to one runner, session, gate, and event sequencer.
    pub fn new_with_event_sequencer(
        runner: Arc<Runner>,
        session: SessionId,
        gate: Arc<dyn houyicoder_permission::ModeGate>,
        event_sequencer: EventSequencer,
    ) -> Self {
        Self {
            runner,
            session,
            event_sequencer,
            replay_after: None,
            next_req_id: 0,
            gate,
            sandbox_session: None,
            file_transaction: None,
            host: None,
            settings_path: houyicoder_config::settings_path(),
            project_path: None,
            provider_facts: None,
            append_notify: None,
            descriptor_store: None,
            diagnostics: handle(),
            bus: None,
            recap: None,
            recap_client: None,
            background_tasks: None,
        }
    }

    /// Attach the session's background command registry, so the tasks
    /// requests answer from the instance the shell tool passes runs to.
    pub fn with_background_tasks(mut self, tasks: Arc<BackgroundTasks>) -> Self {
        self.background_tasks = Some(tasks);
        self
    }

    /// Attach the session's recap state. The composition root passes the one
    /// the session was assembled with; a server built without it never asks
    /// for a summary.
    pub fn with_recap(mut self, recap: Arc<RecapSession>) -> Self {
        self.recap = Some(recap);
        self
    }

    /// Override the settings path the /memory toggle handler persists to.
    /// Tests pass a temp path so a flip never writes the real settings file.
    /// The runner's model resolver is built at composition time from the
    /// global path, so a caller that overrides this one builds the runner over
    /// the same file too - otherwise a catalog row and the capabilities
    /// reported for it come from different files.
    pub fn with_settings_path(mut self, path: PathBuf) -> Self {
        self.settings_path = path;
        self
    }

    /// Set the project workspace path the startup trust prompt gates on.
    /// The composition root passes the resolved cwd so a project session
    /// prompts once before the run loop; a non-project session leaves it
    /// None and skips the prompt.
    pub fn with_project_path(mut self, path: PathBuf) -> Self {
        self.project_path = Some(path);
        self
    }

    /// Attach what the provider was built from. The composition root passes
    /// the facts the resolution produced so /status reports the same ones; a
    /// server assembled without one reports the environment's view.
    pub fn with_provider_facts(mut self, facts: ProviderFacts) -> Self {
        self.provider_facts = Some(facts);
        self
    }

    /// Attach the sandbox session shared with the runner (the /permissions fence).
    pub fn with_session(mut self, session: Arc<dyn SandboxSession>) -> Self {
        self.sandbox_session = Some(session);
        self
    }

    /// Attach the write journal the file tools record through, so the sandbox
    /// pane lists and restores the writes this session made.
    pub fn with_file_transaction(mut self, journal: Arc<FileTransaction>) -> Self {
        self.file_transaction = Some(journal);
        self
    }

    /// Override the diagnostics handle the server read at construction. The
    /// construction path already reads the process-wide handle via
    /// diagnostics::handle(), so this is for tests that install a sink after
    /// building the server, or for explicitly clearing it (passing None) in
    /// a test that must reject /debug.
    pub fn with_diagnostics(mut self, handle: Option<DiagnosticsHandle>) -> Self {
        self.diagnostics = handle;
        self
    }

    /// Tell the user a fact with a consequence they can see. The notice lands
    /// in the transcript, which survives the session, unlike a terminal write
    /// that would be painted over whatever is on screen.
    pub(super) fn notify_user(&self, message: String) {
        self.runner
            .event_handlers()
            .emit_user_notice(UserNoticeEvent { message });
    }

    /// Mint a fresh reverse-request id for a mid-turn server-to-client ask.
    fn mint_req_id(&mut self) -> RequestId {
        let id = RequestId(self.next_req_id);
        self.next_req_id += 1;
        id
    }

    /// Build the /permission git reply: set the toggle when enabled is Some,
    /// then report the resulting state. The toggle now enables/disables the
    /// git-checkpoint builtin rules (no separate flag). Split out of the
    /// dispatch match so the set-then-report logic is unit-testable without
    /// driving the wire.
    fn ask_before_git_response(&self, enabled: Option<bool>) -> ResponsePayload {
        if let Some(e) = enabled {
            self.gate.set_git_checkpoint_enabled(e);
        }
        ResponsePayload::PermissionAskBeforeGit(self.gate.git_checkpoint_enabled())
    }

    /// Build the /model pane snapshot from the live session plus the
    /// settings model section. A missing or malformed section yields defaults
    /// (no catalog), so the pane renders the empty-state guidance rather than
    /// failing. The session side comes from the runner, never from settings:
    /// a session started on another model reports that model until the user
    /// picks again.
    async fn handle_model_info(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
    ) -> Result<(), ProtocolError> {
        let catalog = match self.model_catalog_snapshot().await {
            Ok(catalog) => catalog,
            Err(e) => {
                self.send_response(io, req_id, ResponsePayload::Error(e))
                    .await?;
                return Ok(());
            }
        };
        self.send_response(io, req_id, ResponsePayload::ModelInfo(catalog))
            .await
    }

    /// Run the connection: handshake, then receive request frames until the
    /// client closes. Each request is dispatched; events the run produces are
    /// pushed on the seq stream and the run outcome returns as a response on
    /// the req_id axis. Returns Ok for a clean close, Err for a carrier-level
    /// failure the host surfaces. Owns the I/O so the host can spawn the whole
    /// loop onto a runtime.
    pub async fn serve(mut self, mut io: FrameCarrier) -> Result<(), ProtocolError> {
        let _negotiated = self.handshake(&mut io).await?;
        self.recap_client = self.recap.clone().map(RecapAttachment::attach);
        let trust = self.ensure_trust(&mut io).await?;
        self.runner.set_trust(trust);
        self.replay_events(&mut io).await?;
        session::resume_pending(&mut self, &mut io).await?;
        self.reemit_pending_child_permissions(&mut io).await;
        let mut perm_rx = self
            .bus
            .as_ref()
            .map(|b| b.subscribe(permission_request_topic()));
        loop {
            let wake = self.next_serve_wake(&mut io, perm_rx.as_mut()).await;
            let frame = match wake {
                ServeWake::Event => {
                    self.flush_events(&mut io).await?;
                    continue;
                }
                ServeWake::Recap => {
                    self.start_recap();
                    continue;
                }
                ServeWake::Permission(msg) => {
                    if let Err(e) = child_permission::handle(&mut self, &mut io, msg).await {
                        tracing::warn!("between-run child permission ask failed: {e}");
                    }
                    continue;
                }
                ServeWake::Frame(Ok(Some(frame))) => frame,
                ServeWake::Frame(Ok(None)) => return Ok(()),
                ServeWake::Frame(Err(e)) => return Err(e),
            };
            // A session/* notification between runs: session/inject enqueues
            // a message for the next run's mid-turn drain (a submit that
            // landed as the prior run ended — the race the run-boundary queue
            // catches); session/queue_remove drops a queued message (overlay
            // delete, or popping the head to start a follow-up run).
            // session/cancel is a real no-op here: there is no active run, and
            // routing it to abort() would set the durable aborted flag (which
            // survives across the Interruption boundary) and silently short-
            // circuit the next resume — dropping a later approval with no
            // signal. So cancel is dropped between runs; inject/queue_remove
            // still go through handle_session_notification. A request frame
            // also parses as a notification (the id is ignored), so dispatch
            // by method name + fall through to ClientFrame for anything else
            // — otherwise a message/send would be swallowed.
            if let Ok(notif) = serde_json::from_str::<AcpNotification>(&frame) {
                match notif.method.as_str() {
                    "session/cancel" => continue,
                    "session/inject" | "session/queue_remove" | "session/focus" => {
                        self.handle_session_notification(&notif);
                        continue;
                    }
                    _ => {}
                }
            }
            let client_frame: ClientFrame = match serde_json::from_str(&frame) {
                Ok(cf) => cf,
                Err(e) => {
                    log_bad_frame(&frame, &e);
                    self.send_protocol_error(
                        &mut io,
                        ProtocolError::new(ErrorCategory::InvalidFrame, e.to_string(), false),
                    )
                    .await?;
                    continue;
                }
            };
            let req = match client_frame {
                ClientFrame::Request(req) => req,
                // A bare reverse-response outside a run is unexpected: the
                // serve loop only reads between runs; mid-run responses are
                // consumed by handle_message_send. Fail closed rather than
                // dropping the frame silently.
                ClientFrame::Response(resp) => {
                    self.send_protocol_error(
                        &mut io,
                        ProtocolError::new(
                            ErrorCategory::InvalidFrame,
                            format!("unexpected reverse response for req_id {}", resp.req_id.0),
                            false,
                        ),
                    )
                    .await?;
                    continue;
                }
                // non_exhaustive guard: a future client-frame shape the server
                // does not know yet. Fail closed.
                _ => {
                    self.send_protocol_error(
                        &mut io,
                        ProtocolError::new(
                            ErrorCategory::InvalidFrame,
                            "unknown client frame shape",
                            false,
                        ),
                    )
                    .await?;
                    continue;
                }
            };
            if let Err(e) = self.dispatch(&mut io, req).await {
                // A carrier-level failure (client gone) is fatal to the loop;
                // a per-request protocol error is sent inside dispatch and we
                // continue. Best-effort surface then return.
                self.send_protocol_error(&mut io, e.clone()).await.ok();
                return Err(e);
            }
        }
    }

    /// One step of the between-run select: frames, events, recap, and
    /// permission asks all compete. The permission receiver is shared across
    /// the whole serve loop so a background child's ask between runs reaches
    /// the approval flow without waiting for the next user turn.
    /// Re-emit child permission asks that arrived while no client was
    /// connected. Each becomes a wire Permission card the user sees on
    /// reconnect; the child is still waiting on the response.
    async fn reemit_pending_child_permissions(&mut self, io: &mut FrameCarrier) {
        let pending: Vec<_> = self
            .runner
            .spawn_handle()
            .map(|h| h.take_pending_child_permissions())
            .unwrap_or_default();
        for ask in pending {
            if let Err(e) = child_permission::handle(self, io, permission_request_from(ask)).await {
                tracing::warn!("re-emitted child permission ask failed: {e}");
            }
        }
    }

    async fn next_serve_wake(
        &mut self,
        io: &mut FrameCarrier,
        perm_rx: Option<&mut Receiver<BusMessage>>,
    ) -> ServeWake {
        let append_fut = match &self.append_notify {
            Some(notify) => Either::Left(notify.notified()),
            None => Either::Right(pending::<()>()),
        };
        let event_fut = self.event_sequencer.notified();
        let recap_fut = match self
            .recap
            .as_ref()
            .and_then(|recap| recap.next_due(Instant::now()))
        {
            Some(due) => Either::Left(sleep_until(due)),
            None => Either::Right(pending::<()>()),
        };
        let perm_fut = match perm_rx {
            Some(rx) => Either::Left(rx.recv()),
            None => Either::Right(pending()),
        };
        tokio::select! {
            frame = io.next_frame() => ServeWake::Frame(frame),
            _ = append_fut => ServeWake::Event,
            _ = event_fut => ServeWake::Event,
            _ = recap_fut => ServeWake::Recap,
            msg = perm_fut => match msg {
                Ok(msg) => ServeWake::Permission(msg),
                Err(_) => ServeWake::Event,
            },
        }
    }

    /// Exchange Hello with the client. Both ends send their Hello first; the
    /// server validates the client's version and advertised capabilities. A
    /// version mismatch fails non-retriable so a peer never enters a
    /// half-working session.
    async fn handshake(&mut self, io: &mut FrameCarrier) -> Result<Negotiated, ProtocolError> {
        let local = Hello::local();
        // Send our Hello first so a peer waiting on it can proceed; then read
        // the peer's Hello and validate it.
        Self::send_typed(io, &local).await?;
        let frame = match io.next_frame().await? {
            Some(f) => f,
            None => {
                return Err(ProtocolError::new(
                    ErrorCategory::Unavailable,
                    "client closed before hello",
                    false,
                ));
            }
        };
        let peer: Hello = serde_json::from_str(&frame)
            .map_err(|e| ProtocolError::new(ErrorCategory::InvalidFrame, e.to_string(), false))?;
        self.replay_after = peer.last_event_seq;
        negotiate(&local, &peer)
    }

    /// Drive one user message through the runner. The run streams its turn
    /// events into the session log; on completion the service forwards each
    /// event as a SessionLogEntry frame on the seq stream, then returns the run
    /// outcome as a response correlated to the request.
    ///
    /// Permission asks are surfaced mid-turn as reverse requests: when the
    /// engine returns RunOutcome::Interruption the service does NOT end the
    /// run — it emits one ServerRequestEnvelope::Permission per pending ask,
    /// reads back one ClientResponseEnvelope::Permission per ask, applies the
    /// decisions via runner.resume, and loops. Only when the run ends with a
    /// final outcome does the service send the RunOk response. The half-live
    /// turn state machine lives here (the composition root + protocol server),
    /// not in the runner and not in the wire.
    async fn handle_message_send(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        content: Vec<ContentBlock>,
    ) -> Result<(), ProtocolError> {
        let _turn = self.recap.as_ref().map(|recap| recap.turn_scope());
        self.drive_turn(io, req_id, content).await
    }

    /// The run itself, from the first attempt through every approval round
    /// trip. Split from the caller so the session's recap state brackets it
    /// however it ends.
    async fn drive_turn(
        &mut self,
        io: &mut FrameCarrier,
        req_id: RequestId,
        content: Vec<ContentBlock>,
    ) -> Result<(), ProtocolError> {
        let text: String = content
            .into_iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text),
                _ => None,
            })
            .collect();
        let mut result = self.drive_run_step(io, text).await?;
        loop {
            self.flush_events(io).await?;
            match result {
                Ok(run) => match run.outcome {
                    RunOutcome::Interruption(approvals) => {
                        let decisions = self.collect_interruption_decisions(io, approvals).await?;
                        result = self.drive_resume_step(io, &decisions).await?;
                    }
                    _ => {
                        let payload = ResponsePayload::RunOk(map_run_result(&run));
                        return self.send_response(io, req_id, payload).await;
                    }
                },
                Err(e) => {
                    let payload = ResponsePayload::RunErr(map_run_error(&e));
                    return self.send_response(io, req_id, payload).await;
                }
            }
        }
    }

    /// Start the recap entry that is due and authorized. The request runs on
    /// its own task, so the connection keeps sending frames while the summary
    /// is on its way and the session can cancel it the moment the user returns.
    fn start_recap(&self) {
        let Some(recap) = self.recap.clone() else {
            return;
        };
        let Some(job) = recap.take_due(Instant::now()) else {
            return;
        };
        tokio::spawn(async move {
            let entry = job.entry;
            let _scope = recap.job_scope(entry);
            let outcome = run_recap(recap.as_ref(), entry, job.cancel).await;
            match outcome {
                Outcome::Failed(reason) | Outcome::Refused(reason) => {
                    tracing::warn!(
                        session = %recap.session(),
                        ?entry,
                        %reason,
                        "a recap attempt left no row"
                    );
                }
                other => tracing::debug!(
                    session = %recap.session(),
                    ?entry,
                    ?other,
                    "a recap attempt finished"
                ),
            }
        });
    }

    async fn drive_run_step(
        &mut self,
        io: &mut FrameCarrier,
        text: String,
    ) -> Result<Result<RunResult, RunError>, ProtocolError> {
        let runner = Arc::clone(&self.runner);
        let run_fut = runner.run(self.session, text);
        let mut perm_rx = self
            .bus
            .as_ref()
            .map(|b| b.subscribe(permission_request_topic()));
        self.drive_while(
            io,
            run_fut,
            "client closed mid-run",
            perm_rx.as_mut(),
            OnClose::Abandon,
        )
        .await
    }

    async fn collect_interruption_decisions(
        &mut self,
        io: &mut FrameCarrier,
        approvals: Vec<EngineApprovalRequest>,
    ) -> Result<Vec<EngineApprovalDecision>, ProtocolError> {
        if let Some(host) = &self.host {
            let remaining = approvals
                .iter()
                .map(|a| PendingPermission {
                    call_id: a.call_id.clone(),
                    tool: a.tool_name.clone(),
                    input: a.input.clone(),
                    reason: a.reason.clone(),
                })
                .collect::<Vec<_>>();
            host.store().set_pending(
                self.session,
                Some(PendingTurn {
                    remaining,
                    decided: Vec::new(),
                }),
            );
        }
        let mut decisions = Vec::with_capacity(approvals.len());
        for approval in approvals {
            let decision = self.handle_approval(io, &approval, None).await?;
            decisions.push(decision);
            if self.runner.is_aborted() {
                break;
            }
        }
        if let Some(host) = &self.host {
            host.store().set_pending(self.session, None);
        }
        Ok(decisions)
    }

    pub(super) async fn drive_resume_step(
        &mut self,
        io: &mut FrameCarrier,
        decisions: &[EngineApprovalDecision],
    ) -> Result<Result<RunResult, RunError>, ProtocolError> {
        let runner = Arc::clone(&self.runner);
        let resume_fut = runner.resume(self.session, decisions);
        self.drive_while(
            io,
            resume_fut,
            "client closed mid-resume",
            None,
            OnClose::Abandon,
        )
        .await
    }
}

/// The current wall clock as milliseconds since the Unix epoch. Used for the
/// ts field on a server-appended audit event; falls back to 0 if the clock is
/// before the epoch (not expected outside a misconfigured test harness).
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Log the raw wire line that failed ClientFrame parsing so a debug-on
/// repro captures the offending JSON. The diagnostic sink writes only while
/// the level is raised; with debug off the call is a no-op.
pub(crate) fn log_bad_frame(frame: &str, e: &serde_json::Error) {
    tracing::warn!(raw = frame, error = %e, "client frame parse failed");
}

#[cfg(test)]
mod tests;
