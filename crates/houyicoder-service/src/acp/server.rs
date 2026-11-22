//! The full ACP server: owns the runner + session and drives a turn end to
//! end over the ACP base JSON-RPC dialect. The adapter handles the stateless
//! methods (initialize, session/new, session/load, takeControl,
//! session/cancel); this server owns the stateful one — session/prompt —
//! which runs the engine, streams session/update notifications, and surfaces
//! mid-turn permission asks as session/request_permission reverse requests.
//!
//! The half-live turn state machine mirrors the frontend Server: a run that
//! returns Interruption does NOT end — the server sends one reverse request
//! per pending ask, reads one response back, appends the verdict audit, resumes,
//! and loops. Only a final outcome replies to the original session/prompt id.
//! A session/cancel notification aborts the runner whether it arrives during
//! the run, while a permission card is pending, or between turns.

#![allow(dead_code)] // pub server type consumed by other crates and tests; locally unused

use std::collections::HashSet;
use std::future::pending;
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use crate::acp::adapter::AcpAdapter;
use crate::acp::pump::AcpDuplexPump;
use crate::acp::pump::InboundFrame;
use crate::consent::{ChildPermissionRx, ConsentPolicy};
use crate::lifecycle::{CLOSE_SETTLE_BUDGET, settle_on_disconnect};
use crate::protocol_adapter::{
    map_run_error, map_run_result, map_session_update, map_stream_event, tool_title,
};
use crate::turn_events::TurnEventSubscriber;
use futures::future::Either;
use houyicoder_api::agent_event::ResponseStreamEvent;
use houyicoder_api::session::TrajectoryPages;
use houyicoder_api::session::TrajectoryPosition;
use houyicoder_context::{SessionEvent, SessionId, SessionLogEntry};
use houyicoder_core::agent::RunError;
use houyicoder_core::agent::RunResult;
use houyicoder_core::agent::multi_agent::bus_types::AgentBus;
use houyicoder_core::agent::{ApprovalDecision, RunOutcome, Runner};
use houyicoder_permission::ModeGate;
use houyicoder_protocol::acp_wire::{
    AcpErrorCode, AcpRequest, AcpRequestId, AcpResponse, PromptRequest, PromptResponse,
};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::session_update::SessionUpdate;
use houyicoder_protocol::llm::Usage;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// The full ACP server. Owns the adapter (shared via Arc so the composition
/// root and the IO bridge can hold the same one) plus the runner + session +
/// the reverse-request id counter + the pushed-event cursor. serve runs the
/// frame loop: a request drives the runner (session/prompt) or delegates to
/// the adapter, a notification reaps state and aborts the runner
/// (session/cancel), a response wakes the reverse-request waiter, and an
/// unparseable line is answered with a parse error unless a run is active,
/// where it fails closed.
pub struct AcpServer {
    adapter: Arc<AcpAdapter>,
    runner: Arc<Runner>,
    gate: Arc<dyn ModeGate>,
    session: SessionId,
    next_req_id: u64,
    /// The multi-agent bus a child's permission ask arrives on, and the
    /// connection-scoped receiver that consumes it. None on the single-agent
    /// path: no bus means no child can publish, so no ask arrives.
    bus: Option<Arc<AgentBus>>,
    child_perm_rx: Arc<ChildPermissionRx>,
    /// The position the client has been pushed up to: every durable record
    /// before it was sent. An absolute ordinal of the session's own history,
    /// so it survives the log state dropping its oldest rows.
    pushed_through: TrajectoryPosition,
    /// The run's live events, when the composition root subscribed this
    /// connection to them. None when the runner has no stream installed; the
    /// serve loop sends only the durable events then.
    events: Option<TurnEventSubscriber>,
    /// Pacing for tool progress updates, so a fast-reporting tool cannot
    /// fill the client stream with snapshots it cannot render.
    progress: ToolProgressThrottle,
    /// True once a live delta was sent this turn, so the durable Assistant
    /// and Reasoning events are suppressed to avoid sending the turn text
    /// twice (as deltas and again as a whole message).
    streamed_text: AtomicBool,
    /// True once this turn's live stream dropped events. The suppression
    /// above then stops applying: a client missing deltas needs the durable
    /// copy, which is the only remaining place the text exists.
    stream_lossy: bool,
    /// Tool calls whose durable result was already pushed this turn. A
    /// progress snapshot that arrives after that must not overwrite the
    /// completed status with a running one.
    completed_calls: HashSet<String>,
    /// The model ids the client may switch to through session/set_config_option.
    /// Empty when the composition root supplied no catalog; a switch is then
    /// rejected rather than silently accepted for an unknown id.
    model_options: Vec<String>,
    /// Set when a session/cancel notification arrives during a pending
    /// permission ask (the run is paused at Interruption, the drive loop
    /// is not streaming). The resume loop checks this after each frame read
    /// and breaks out — the turn ends with a cancelled stop_reason instead
    /// of waiting for a verdict the client will never send. (runner.abort()
    /// cancels the in-flight stream loop via the token; the durable flag
    /// only fires when the run is paused on an ask, so an idle cancel is a
    /// no-op on the flag.)
    cancel_requested: AtomicBool,
}

/// The turn result a cancel during a permission ask produces: the turn ended
/// before the engine could continue.
fn cancelled_run() -> RunResult {
    RunResult {
        outcome: RunOutcome::Interrupted("cancelled during permission ask".to_string()),
        turns: 0,
        usage: Usage::default(),
    }
}

impl AcpServer {
    /// Build a server bound to one runner + session + mode gate, delegating
    /// the stateless methods to the adapter. The composition root constructs
    /// the runner and shares the adapter via Arc.
    pub fn new(
        adapter: Arc<AcpAdapter>,
        runner: Arc<Runner>,
        gate: Arc<dyn ModeGate>,
        session: SessionId,
    ) -> Self {
        Self {
            adapter,
            runner,
            gate,
            session,
            next_req_id: 0,
            bus: None,
            child_perm_rx: Arc::new(ChildPermissionRx::default()),
            pushed_through: TrajectoryPosition::start(),
            events: None,
            progress: ToolProgressThrottle::new(),
            streamed_text: AtomicBool::new(false),
            stream_lossy: false,
            completed_calls: HashSet::new(),
            model_options: Vec::new(),
            cancel_requested: AtomicBool::new(false),
        }
    }

    /// Wire the shared multi-agent bus so a child's permission ask reaches the
    /// ACP approval flow. None on the single-agent path.
    pub fn with_bus(mut self, bus: Option<Arc<AgentBus>>) -> Self {
        self.bus = bus;
        self
    }

    /// Build a consent policy from this server's gate, sandbox session, audit
    /// log, skill registry, and session id, so the ACP path installs the same
    /// grant for the same answer the frontend server does.
    pub(super) fn consent(&self) -> ConsentPolicy {
        ConsentPolicy::new(
            self.gate.clone(),
            self.runner.sandbox_session(),
            self.runner.store(),
            self.runner.skill_registry().cloned(),
            self.session,
        )
    }

    /// Subscribe this connection to the run's live events. The stdio path
    /// installs the producer on the runner before wrapping it in Arc and
    /// hands the subscriber here; an attached connection forwards the host's
    /// own event sources into a subscriber instead. The serve loop sends what
    /// it receives while a turn is live.
    pub fn with_events(mut self, events: TurnEventSubscriber) -> Self {
        self.events = Some(events);
        self
    }

    /// Replace the interval between tool progress updates. A test sets it
    /// explicitly so a burst can be held inside one window.
    #[cfg(test)]
    pub(super) fn with_progress_interval(mut self, interval: Duration) -> Self {
        self.progress.set_interval(interval);
        self
    }

    /// Set the model ids a client may switch to through
    /// session/set_config_option. Built by the composition root from the
    /// settings catalog plus the runner's active model.
    pub fn with_model_options(mut self, models: Vec<String>) -> Self {
        self.model_options = models;
        self
    }

    /// Run the connection: receive frames until the client closes. Each frame
    /// arrives classified by the pump; session/prompt drives the runner
    /// through handle_prompt, other requests delegate to the adapter and the
    /// reply is written back, and a response to a reverse request routes to
    /// its waiter. Returns Ok for a clean close, Err for a pump-level
    /// failure.
    pub async fn serve(mut self, pump: &mut AcpDuplexPump) -> Result<(), ProtocolError> {
        let perm_owner = Arc::clone(&self.child_perm_rx);
        loop {
            // A child's permission ask can arrive while the host is idle
            // between prompts, not only mid-run. Race the next inbound frame
            // against the permission receiver so a child is never left
            // waiting for the next prompt to start a drive loop.
            let mut perm_lease = perm_owner.lease(self.bus.as_ref());
            let permission_fut = match perm_lease.receiver() {
                Some(receiver) => Either::Left(receiver.recv()),
                None => Either::Right(pending()),
            };
            tokio::select! {
                biased;
                frame = pump.next_frame() => {
                    let Some(frame) = frame else {
                        return Ok(());
                    };
                    match frame {
                        InboundFrame::Request(req) => match req.method.as_str() {
                            "session/prompt" => {
                                if let Err(e) = self.handle_prompt(pump, &req).await {
                                    self.send_protocol_error(pump, e.clone()).await.ok();
                                    return Err(e);
                                }
                            }
                            "session/set_mode" => self.handle_set_mode(pump, &req).await?,
                            "session/set_config_option" => {
                                self.handle_set_config_option(pump, &req).await?
                            }
                            _ => {
                                let resp = self.adapter.handle(&req).await;
                                self.send_response(pump, &resp).await?;
                            }
                        },
                        InboundFrame::Notification(notif) => {
                            // An idle cancel has no drive loop to observe the
                            // token, so abort here: the next run starts
                            // un-cancelled, and a cancel that races a run's
                            // start still reaches it.
                            if notif.method == "session/cancel" {
                                self.runner.abort();
                            }
                            self.adapter.handle_notification(&notif).await;
                        }
                        InboundFrame::Response(resp) => {
                            if !pump.deliver_response(resp) {
                                tracing::warn!("response matched no pending reverse request");
                            }
                        }
                        InboundFrame::Unparseable(line) => {
                            // JSON-RPC 2.0 answers a frame that is not valid
                            // JSON-RPC with a parse error on the null id, so
                            // the peer learns the frame was rejected rather
                            // than waiting for a reply.
                            tracing::warn!(bytes = line.len(), "inbound frame did not parse");
                            let resp = AcpResponse::err(
                                AcpRequestId::Null,
                                AcpErrorCode::ParseError,
                                "frame did not parse as a request, response, or notification",
                            );
                            self.send_response(pump, &resp).await?;
                        }
                    }
                }
                msg = permission_fut => {
                    if let Ok(msg) = msg
                        && let Err(e) = self.route_child_permission(pump, msg).await
                    {
                        tracing::warn!("child permission ask failed: {e}");
                    }
                }
            }
        }
    }

    /// Drive one session/prompt request through the runner. The run streams
    /// its turn events as session/update notifications; on Interruption the
    /// server sends a session/request_permission reverse request per pending
    /// ask, reads the response, appends the verdict audit, and resumes. Only a
    /// final outcome replies to the original request id with a PromptResponse.
    #[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
    async fn handle_prompt(
        &mut self,
        io: &mut AcpDuplexPump,
        req: &AcpRequest,
    ) -> Result<(), ProtocolError> {
        let params: PromptRequest = match req.params.as_ref() {
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(p) => p,
                Err(e) => {
                    return self
                        .send_response(
                            io,
                            &AcpResponse::err(
                                req.id.clone(),
                                AcpErrorCode::InvalidParams,
                                format!("bad params: {e}"),
                            ),
                        )
                        .await;
                }
            },
            None => {
                return self
                    .send_response(
                        io,
                        &AcpResponse::err(
                            req.id.clone(),
                            AcpErrorCode::InvalidParams,
                            "missing params",
                        ),
                    )
                    .await;
            }
        };
        // Fail closed when the wire session id does not name this server's
        // session — a stock ACP client obtains sessionId from session/new and
        // must prompt against that same id; a mismatch is a routing error,
        // not a silent run on the wrong session. (Mirrors the frontend
        // Server.session_matches gate.)
        if params.session_id != self.session.to_string() {
            return self
                .send_response(
                    io,
                    &AcpResponse::err(
                        req.id.clone(),
                        AcpErrorCode::InvalidParams,
                        format!("session id mismatch: got {}", params.session_id),
                    ),
                )
                .await;
        }
        // Each turn begins un-cancelled: a cancel that ended the previous turn
        // must not leak into this one. A cancel queued before this prompt still
        // reaches the run select below, so resetting here loses nothing.
        self.cancel_requested.store(false, Ordering::Relaxed);
        self.streamed_text.store(false, Ordering::Relaxed);
        self.progress.reset();
        self.stream_lossy = false;
        self.completed_calls.clear();
        // The engine run takes plain text today. A non-text block is not silently
        // dropped: the agent declared promptCapabilities.image false at
        // initialize, so a client that sends one is told which block it must
        // not send, and an empty prompt is not fed to the runner.
        let text = match prompt_text(params.prompt) {
            Ok(text) => text,
            Err(msg) => {
                return self
                    .send_response(
                        io,
                        &AcpResponse::err(req.id.clone(), AcpErrorCode::InvalidParams, msg),
                    )
                    .await;
            }
        };
        // The client's own message goes out before the run starts. Its durable
        // record is only readable once the run has appended it, so echoing it
        // here is what keeps the conversation in the order the user typed it;
        // the durable copy is suppressed below.
        self.echo_user_prompt(io, &text).await?;
        // Await the run while concurrently reading inbound frames so a
        // session/cancel notification arriving mid-run aborts the drive loop.
        // Any other frame mid-run is a protocol violation (fail-closed).
        let mut result = self.drive_run(io, text).await?;
        loop {
            self.push_pending(io).await?;
            match result {
                Ok(run) => match run.outcome {
                    RunOutcome::Interruption(approvals) => {
                        let Some(decisions) = self.ask_permissions(io, approvals, None).await?
                        else {
                            tracing::debug!("permission ask cancelled; turn ends without resume");
                            result = Ok(cancelled_run());
                            continue;
                        };
                        result = self.resume_run(io, &decisions).await?;
                        continue;
                    }
                    _ => {
                        let run_view = map_run_result(&run);
                        let resp = PromptResponse {
                            stop_reason: run_view.stop_reason,
                            meta: None,
                        };
                        return self
                            .send_response(
                                io,
                                &AcpResponse::ok(
                                    req.id.clone(),
                                    serde_json::to_value(resp).expect("prompt response serialize"),
                                ),
                            )
                            .await;
                    }
                },
                Err(e) => {
                    let _run_error = map_run_error(&e);
                    return self
                        .send_response(
                            io,
                            &AcpResponse::err(
                                req.id.clone(),
                                AcpErrorCode::InternalError,
                                format!("{e}"),
                            ),
                        )
                        .await;
                }
            }
        }
    }

    /// Drive the run future to completion while reading inbound frames and live
    /// events concurrently. A session/cancel notification aborts the run; any
    /// other frame mid-run is a protocol violation. The subscriber is taken
    /// out of self so its borrow does not collide with the &self calls inside
    /// the select, and restored on the normal path. Returns the run's own
    /// outcome inside a transport result: the outer error ends the serve loop.
    async fn drive_run(
        &mut self,
        io: &mut AcpDuplexPump,
        text: String,
    ) -> Result<Result<RunResult, RunError>, ProtocolError> {
        // The run future holds its own handle, so the loop can still reach
        // the durable trail through the server while the turn is live.
        let runner = Arc::clone(&self.runner);
        let run_fut = runner.run(self.session, text);
        tokio::pin!(run_fut);
        let mut events = self.events.take();
        let perm_owner = Arc::clone(&self.child_perm_rx);
        let mut perm_lease = perm_owner.lease(self.bus.as_ref());
        // biased: poll the run future first so a ready run resolves before an
        // inbound EOF frame wins the race (without this, a fast stub run plus
        // an immediate stdin close can drop the real reply for a spurious
        // client-closed error).
        let outcome = loop {
            let permission_fut = match perm_lease.receiver() {
                Some(receiver) => Either::Left(receiver.recv()),
                None => Either::Right(pending()),
            };
            tokio::select! {
                biased;
                r = &mut run_fut => break r,
                event = next_turn_event(&mut events) => {
                    self.note_stream_lag(&mut events);
                    self.push_stream_event(io, &event).await?;
                }
                frame = io.next_frame() => match frame {
                    Some(f) => {
                        if !self.route_cancel_frame(&f).await? {
                            self.events = events;
                            return Err(ProtocolError::new(
                                ErrorCategory::InvalidFrame,
                                "unexpected frame mid-run",
                                false,
                            ));
                        }
                    }
                    None => {
                        // Settle the in-flight run before failing the
                        // connection: a tool that already appended its call
                        // gets the budget to append its paired result, so the
                        // durable trail closes and the next resume sees no
                        // orphan.
                        settle_on_disconnect(&mut run_fut, CLOSE_SETTLE_BUDGET, || self.runner.abort()).await;
                        self.events = events;
                        return Err(ProtocolError::new(
                            ErrorCategory::Unavailable,
                            "client closed mid-run",
                            false,
                        ));
                    }
                },
                req = permission_fut => {
                    if let Ok(msg) = req
                        && let Err(e) = self.route_child_permission(io, msg).await
                    {
                        tracing::warn!("child permission ask failed: {e}");
                    }
                },
            }
        };
        // Drain events the model pushed in its final frame before the run
        // future resolved, so the tail is not lost to streamed_text suppression.
        self.drain_event_tail(io, &mut events).await?;
        self.events = events;
        Ok(outcome)
    }

    /// Drive the resumed turn and stream its live events. A cancel that arrived
    /// during the ask skips resume and ends the turn cancelled.
    async fn resume_run(
        &mut self,
        io: &mut AcpDuplexPump,
        decisions: &[ApprovalDecision],
    ) -> Result<Result<RunResult, RunError>, ProtocolError> {
        if self.cancel_requested.load(Ordering::Relaxed) {
            return Ok(Ok(cancelled_run()));
        }
        let runner = Arc::clone(&self.runner);
        let resume_fut = runner.resume(self.session, decisions);
        tokio::pin!(resume_fut);
        let mut events = self.events.take();
        let perm_owner = Arc::clone(&self.child_perm_rx);
        let mut perm_lease = perm_owner.lease(self.bus.as_ref());
        let outcome = loop {
            let permission_fut = match perm_lease.receiver() {
                Some(receiver) => Either::Left(receiver.recv()),
                None => Either::Right(pending()),
            };
            tokio::select! {
                biased;
                r = &mut resume_fut => break r,
                event = next_turn_event(&mut events) => {
                    self.note_stream_lag(&mut events);
                    self.push_stream_event(io, &event).await?;
                }
                frame = io.next_frame() => match frame {
                    Some(f) => {
                        if !self.route_cancel_frame(&f).await? {
                            self.events = events;
                            return Err(ProtocolError::new(
                                ErrorCategory::InvalidFrame,
                                "unexpected frame mid-resume",
                                false,
                            ));
                        }
                    }
                    None => {
                        settle_on_disconnect(&mut resume_fut, CLOSE_SETTLE_BUDGET, || self.runner.abort()).await;
                        self.events = events;
                        return Err(ProtocolError::new(
                            ErrorCategory::Unavailable,
                            "client closed mid-resume",
                            false,
                        ));
                    }
                },
                req = permission_fut => {
                    if let Ok(msg) = req
                        && let Err(e) = self.route_child_permission(io, msg).await
                    {
                        tracing::warn!("child permission ask failed: {e}");
                    }
                },
            }
        };
        self.drain_event_tail(io, &mut events).await?;
        self.events = events;
        Ok(outcome)
    }

    /// Inspect one inbound frame read mid-run (or mid-resume) for a
    /// session/cancel notification. Returns Ok(true) when the frame was a
    /// cancel (the run future keeps polling, the drive loop observes the
    /// cancelled token, and resolves Interrupted) and Ok(false) for any other
    /// frame, which the caller treats as a protocol violation (fail-closed).
    /// The store is reaped so lifecycle state reflects Cancelled. A cancel
    /// arriving while a permission card is pending is handled by the ask
    /// wait, not here.
    async fn route_cancel_frame(&self, frame: &InboundFrame) -> Result<bool, ProtocolError> {
        let InboundFrame::Notification(notif) = frame else {
            return Ok(false);
        };
        if notif.method != "session/cancel" {
            return Ok(false);
        }
        self.runner.abort();
        self.adapter.handle_notification(notif).await;
        self.cancel_requested.store(true, Ordering::Relaxed);
        Ok(true)
    }

    /// The session's durable records past the position the client was last
    /// pushed up to, in append order.
    ///
    /// The read starts at an absolute position in the session's own history
    /// rather than counting from the front of a returned window: the store
    /// keeps only its newest rows in memory, so counting from a window would
    /// re-send records the client already saw as soon as the window moved. A
    /// read that fails leaves the position after the records it did report, so
    /// the next pass resumes there instead of repeating them.
    fn drain_pushed(&mut self) -> Vec<SessionLogEntry> {
        let store = self.runner.store();
        let pages = TrajectoryPages::new(store.as_ref(), self.session, self.pushed_through);
        let mut events = Vec::new();
        for page in pages {
            match page {
                Ok(page) => {
                    self.pushed_through = page.next;
                    events.extend(page.events);
                }
                Err(e) => {
                    tracing::warn!("durable read for session {} stopped: {e}", self.session);
                    break;
                }
            }
        }
        events
    }

    /// Push every durable turn event the client has not seen yet, in the order
    /// the engine appended them.
    async fn push_pending(&mut self, io: &mut AcpDuplexPump) -> Result<(), ProtocolError> {
        let events = self.drain_pushed();
        for ev in &events {
            self.push_durable_event(io, ev).await?;
        }
        Ok(())
    }

    /// Echo the prompt the client just sent as a user message chunk, so the
    /// host renders it before the answer rather than after it.
    async fn echo_user_prompt(
        &self,
        io: &mut AcpDuplexPump,
        text: &str,
    ) -> Result<(), ProtocolError> {
        let event = SessionEvent::UserInput {
            text: text.to_string(),
        };
        if let Some(update) = map_session_update(&event) {
            let notification = session_update(self.session, &update)?;
            self.send_typed(io, &notification).await?;
        }
        Ok(())
    }

    /// Forward one engine turn event as a session/update notification. Kinds
    /// the base protocol has no standard counterpart for (compaction boundary,
    /// summary, meta user, permission decision) ride the acpx/context/* side
    /// channel — that mapping lands with the acpx serve integration; a
    /// pure-ACP client ignores it anyway, so the base stream here is complete
    /// for the first cut.
    async fn push_durable_event(
        &mut self,
        io: &mut AcpDuplexPump,
        ev: &SessionLogEntry,
    ) -> Result<(), ProtocolError> {
        // The prompt was echoed when it arrived, so its durable record would
        // repeat the user's message.
        if matches!(ev.event, SessionEvent::UserInput { .. }) {
            return Ok(());
        }
        // A finished call is remembered so a progress snapshot that was
        // already queued cannot downgrade its status back to running.
        if let SessionEvent::ToolResult { call_id, .. } = &ev.event {
            self.completed_calls.insert(call_id.clone());
        }
        // When the live stream carried the turn text as deltas, the durable
        // Assistant and Reasoning events would repeat it as a whole message;
        // suppress them so the client does not render the turn twice. Other
        // durable events (tool calls, results, run completion) still push.
        if self.suppresses_durable(&ev.event) {
            return Ok(());
        }
        if let Some(mut update) = map_session_update(&ev.event) {
            // The frontend keeps its own short tool labels; the ACP client
            // gets the human title its card protocol defines.
            if let (SessionEvent::ToolCall { tool, input, .. }, SessionUpdate::ToolCall(call)) =
                (&ev.event, &mut update)
            {
                call.title = tool_title(tool, Some(input));
            }
            let notification = session_update(self.session, &update)?;
            self.send_typed(io, &notification).await?;
        }
        Ok(())
    }

    /// Whether a durable turn record repeats text the live stream already sent
    /// and must therefore be suppressed. A turn whose stream dropped events
    /// keeps its durable copy: the client is missing text, and this is where
    /// the full message is.
    fn suppresses_durable(&self, event: &SessionEvent) -> bool {
        !self.stream_lossy
            && self.streamed_text.load(Ordering::Relaxed)
            && matches!(
                event,
                SessionEvent::AssistantMessage { .. } | SessionEvent::Reasoning { .. }
            )
    }

    /// Forward one live response delta as a session/update notification. Text
    /// deltas become agent message chunks, reasoning deltas become thought
    /// chunks; tool-input deltas carry raw JSON and stay off the client
    /// stream. The first text delta marks the turn as streamed so the
    /// durable copy is suppressed.
    async fn push_delta(
        &self,
        io: &mut AcpDuplexPump,
        delta: &ResponseStreamEvent,
    ) -> Result<(), ProtocolError> {
        // Any text or reasoning delta marks the turn as streamed, so the
        // durable Assistant and Reasoning events are both suppressed. A
        // reasoning-only turn still streams its reasoning; without this flag
        // the durable copy would repeat it.
        if matches!(
            delta,
            ResponseStreamEvent::AssistantTextDelta { .. }
                | ResponseStreamEvent::ReasoningDelta { .. }
        ) {
            self.streamed_text.store(true, Ordering::Relaxed);
        }
        if let Some(update) = map_stream_event(delta) {
            let notification = session_update(self.session, &update)?;
            self.send_typed(io, &notification).await?;
        }
        Ok(())
    }

    /// Send a response paired to a request by id.
    async fn send_response(
        &self,
        io: &mut AcpDuplexPump,
        resp: &AcpResponse,
    ) -> Result<(), ProtocolError> {
        self.send_typed(io, resp).await
    }

    /// Send a protocol error with no correlation (the req_id is unknown).
    async fn send_protocol_error(
        &self,
        io: &mut AcpDuplexPump,
        err: ProtocolError,
    ) -> Result<(), ProtocolError> {
        let resp = AcpResponse::err(
            AcpRequestId::Null,
            AcpErrorCode::InternalError,
            err.to_string(),
        );
        self.send_typed(io, &resp).await
    }

    /// Encode a typed message and push it through the carrier.
    async fn send_typed<T: serde::Serialize>(
        &self,
        io: &mut AcpDuplexPump,
        msg: &T,
    ) -> Result<(), ProtocolError> {
        let frame = encode(msg)?;
        io.send_frame(frame).await
    }
}

mod config;
mod permission;
mod progress;
mod stream;
mod util;

use progress::ToolProgressThrottle;
use util::next_turn_event;
use util::prompt_text;
use util::session_update;

#[cfg(test)]
mod tests;
