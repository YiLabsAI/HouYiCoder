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

use std::sync::Arc;

use crate::acp::adapter::AcpAdapter;
use crate::acp::live::AcpLiveStream;
use crate::acp::pump::AcpDuplexPump;
use crate::acp::pump::InboundFrame;
use crate::protocol_adapter::{
    acp_permission_response_to_decision, approval_to_acp_permission, map_run_error, map_run_result,
    map_session_update, map_stream_event,
};
use houyicoder_api::agent_event::ResponseStreamEvent;
use houyicoder_api::session::TrajectoryPages;
use houyicoder_api::session::TrajectoryPosition;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_core::agent::RunError;
use houyicoder_core::agent::RunResult;
use houyicoder_core::agent::{ApprovalRequest, RunOutcome, Runner};
use houyicoder_permission::ModeGate;
use houyicoder_protocol::acp_wire::{
    AcpErrorCode, AcpNotification, AcpRequest, AcpRequestId, AcpResponse, PromptRequest,
    PromptResponse, RequestPermissionResponse,
};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::llm::Usage;
use std::future::pending;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

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
    /// The position the client has been pushed up to: every durable record
    /// before it was sent. An absolute ordinal of the session's own history,
    /// so it survives the log state dropping its oldest rows.
    pushed_through: TrajectoryPosition,
    /// The live model-output stream, when the composition root installed one
    /// on the runner. None in tests that drive a stub provider without a
    /// stream sink; the serve loop sends only the durable events then.
    live: Option<AcpLiveStream>,
    /// True once a live delta was sent this turn, so the durable Assistant
    /// and Reasoning events are suppressed to avoid sending the turn text
    /// twice (as deltas and again as a whole message).
    streamed_text: AtomicBool,
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
            pushed_through: TrajectoryPosition::start(),
            live: None,
            streamed_text: AtomicBool::new(false),
            model_options: Vec::new(),
            cancel_requested: AtomicBool::new(false),
        }
    }

    /// Install the live model-output stream the runner reports deltas
    /// through. The composition root builds the pair and installs the sink
    /// on the runner before wrapping it in Arc; the serve loop drains the
    /// receiver this holds.
    pub fn with_live_stream(mut self, live: AcpLiveStream) -> Self {
        self.live = Some(live);
        self
    }

    /// Set the model ids a client may switch to through
    /// session/set_config_option. Built by the composition root from the
    /// settings catalog plus the runner's active model.
    pub fn with_model_options(mut self, models: Vec<String>) -> Self {
        self.model_options = models;
        self
    }

    /// Mint a fresh reverse-request id for a mid-turn permission ask. Distinct
    /// from any event seq so the two correlation axes never collide.
    fn mint_req_id(&mut self) -> i64 {
        let id = self.next_req_id as i64;
        self.next_req_id += 1;
        id
    }

    /// Append the durable record of one approval card this transport sent. A
    /// failed write is logged rather than failing the ask: the user still
    /// needs the question, and the fold reports the cards it did see.
    async fn record_asked(&self, approval: &ApprovalRequest) {
        let record = SessionLogEntry {
            id: EventId::new(),
            session: self.session,
            ts: now_millis(),
            prev_hash: None,
            event: SessionEvent::PermissionAsked {
                call_id: approval.call_id.clone(),
                tool: approval.tool_name.clone(),
            },
        };
        if let Err(e) = self.runner.store().append(record).await {
            tracing::warn!("permission-asked audit append failed: {e}");
        }
    }

    /// Run the connection: receive frames until the client closes. Each frame
    /// arrives classified by the pump; session/prompt drives the runner
    /// through handle_prompt, other requests delegate to the adapter and the
    /// reply is written back, and a response to a reverse request routes to
    /// its waiter. Returns Ok for a clean close, Err for a pump-level
    /// failure.
    pub async fn serve(mut self, pump: &mut AcpDuplexPump) -> Result<(), ProtocolError> {
        loop {
            let Some(frame) = pump.next_frame().await else {
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
                    // An idle cancel has no drive loop to observe the token,
                    // so abort here: the next run starts un-cancelled, and a
                    // cancel that races a run's start still reaches it.
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
                    // JSON-RPC 2.0 answers a frame that is not valid JSON-RPC
                    // with a parse error on the null id, so the peer learns
                    // the frame was rejected rather than waiting for a reply.
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
        // Await the run while concurrently reading inbound frames so a
        // session/cancel notification arriving mid-run aborts the drive loop.
        // Any other frame mid-run is a protocol violation (fail-closed).
        let mut result = self.drive_run(io, text).await?;
        loop {
            // Push only the events appended since the last push so a resumed
            // run does not re-send what the client already saw.
            let events = self.drain_pushed();
            for ev in &events {
                self.push_turn_event(io, ev).await?;
            }
            match result {
                Ok(run) => match run.outcome {
                    RunOutcome::Interruption(approvals) => {
                        let mut decisions = Vec::with_capacity(approvals.len());
                        let mut cancelled = false;
                        for approval in approvals {
                            let ask_id = self.mint_req_id();
                            let session_str = self.session.to_string();
                            let params = approval_to_acp_permission(&approval, session_str);
                            let params_value = serde_json::to_value(&params).map_err(|e| {
                                ProtocolError::new(
                                    ErrorCategory::InvalidFrame,
                                    e.to_string(),
                                    false,
                                )
                            })?;
                            let ask =
                                AcpRequest::new(ask_id, "session/request_permission", params_value);
                            self.send_typed(io, &ask).await?;
                            // The card is on the durable trail from the moment
                            // the client received it: an ask abandoned or
                            // cancelled at the client records no verdict, and
                            // the audit must still hold the question rather
                            // than report that none was ever asked.
                            self.record_asked(&approval).await;
                            // A cancel that arrives while the card is up ends
                            // the turn without a verdict: the remaining asks
                            // are not sent and resume is skipped below.
                            let Some(resp) = self.await_permission(io, ask_id).await? else {
                                cancelled = true;
                                break;
                            };
                            let resp_value = match resp {
                                AcpResponse::Result { result, .. } => result,
                                AcpResponse::Error { error, .. } => {
                                    return Err(ProtocolError::new(
                                        ErrorCategory::InvalidFrame,
                                        format!("permission ask rejected: {error:?}"),
                                        false,
                                    ));
                                }
                            };
                            let perm_resp: RequestPermissionResponse =
                                serde_json::from_value(resp_value).map_err(|e| {
                                    ProtocolError::new(
                                        ErrorCategory::InvalidFrame,
                                        e.to_string(),
                                        false,
                                    )
                                })?;
                            // Record the durable PermissionDecision audit event
                            // before resume applies the decision. A cancelled
                            // response carries no verdict, so the card stays
                            // on the trail as an outcome nobody stated.
                            let reply = acp_permission_response_to_decision(
                                perm_resp,
                                approval.call_id.clone(),
                            );
                            if let Some(verdict) = reply.verdict {
                                let audit = SessionLogEntry {
                                    id: EventId::new(),
                                    session: self.session,
                                    ts: now_millis(),
                                    prev_hash: None,
                                    event: SessionEvent::PermissionDecision {
                                        call_id: approval.call_id.clone(),
                                        tool: approval.tool_name.clone(),
                                        verdict,
                                        scope: "once".into(),
                                        validator: None,
                                        reason: None,
                                    },
                                };
                                if let Err(e) = self.runner.store().append(audit).await {
                                    tracing::warn!("permission-decision audit append failed: {e}");
                                }
                            }
                            decisions.push(reply.decision);
                        }
                        if cancelled {
                            tracing::debug!("permission ask cancelled; turn ends without resume");
                        }
                        result = {
                            let resume_fut = self.runner.resume(self.session, &decisions);
                            tokio::pin!(resume_fut);
                            let mut live = self.live.take();
                            let outcome = loop {
                                // If a cancel arrived during the permission
                                // ask (route_cancel_frame set the flag), skip
                                // resume entirely — the turn ends cancelled.
                                if self.cancel_requested.load(Ordering::Relaxed) {
                                    break Ok(RunResult {
                                        outcome: RunOutcome::Interrupted(
                                            "cancelled during permission ask".to_string(),
                                        ),
                                        turns: 0,
                                        usage: Usage::default(),
                                    });
                                }
                                tokio::select! {
                                    biased;
                                    r = &mut resume_fut => break r,
                                    delta = next_delta(&mut live) => {
                                        if let Some(delta) = delta {
                                            self.push_delta(io, &delta).await?;
                                        }
                                    }
                                    frame = io.next_frame() => match frame {
                                        Some(f) => {
                                            if !self.route_cancel_frame(&f).await? {
                                                return Err(ProtocolError::new(
                                                    ErrorCategory::InvalidFrame,
                                                    "unexpected frame mid-resume",
                                                    false,
                                                ));
                                            }
                                        }
                                        None => {
                                            return Err(ProtocolError::new(
                                                ErrorCategory::Unavailable,
                                                "client closed mid-resume",
                                                false,
                                            ));
                                        }
                                    },
                                }
                            };
                            self.live = live;
                            outcome
                        };
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
    /// deltas concurrently. A session/cancel notification aborts the run; any
    /// other frame mid-run is a protocol violation. The live stream is taken
    /// out of self so its borrow does not collide with the &self calls inside
    /// the select, and restored on the normal path. Returns the run's own
    /// outcome inside a transport result: the outer error ends the serve loop.
    async fn drive_run(
        &mut self,
        io: &mut AcpDuplexPump,
        text: String,
    ) -> Result<Result<RunResult, RunError>, ProtocolError> {
        let run_fut = self.runner.run(self.session, text);
        tokio::pin!(run_fut);
        let mut live = self.live.take();
        // biased: poll the run future first so a ready run resolves before an
        // inbound EOF frame wins the race (without this, a fast stub run plus
        // an immediate stdin close can drop the real reply for a spurious
        // client-closed error).
        let outcome = loop {
            tokio::select! {
                biased;
                r = &mut run_fut => break r,
                delta = next_delta(&mut live) => {
                    if let Some(delta) = delta {
                        self.push_delta(io, &delta).await?;
                    }
                }
                frame = io.next_frame() => match frame {
                    Some(f) => {
                        if !self.route_cancel_frame(&f).await? {
                            self.live = live;
                            return Err(ProtocolError::new(
                                ErrorCategory::InvalidFrame,
                                "unexpected frame mid-run",
                                false,
                            ));
                        }
                    }
                    None => {
                        self.live = live;
                        return Err(ProtocolError::new(
                            ErrorCategory::Unavailable,
                            "client closed mid-run",
                            false,
                        ));
                    }
                },
            }
        };
        self.live = live;
        Ok(outcome)
    }

    /// Wait for the response to one permission ask. Returns None when a
    /// session/cancel notification arrived first: the turn ends cancelled and
    /// the ask keeps no verdict. The response is matched by its correlation
    /// id through the pump's waiter table, so a reply meant for another ask
    /// stays pending instead of deciding this card. An unparseable line is
    /// answered with a parse error and the wait continues; any other frame
    /// while the card is up is a protocol violation.
    async fn await_permission(
        &mut self,
        io: &mut AcpDuplexPump,
        ask_id: i64,
    ) -> Result<Option<AcpResponse>, ProtocolError> {
        let mut response = io.expect_response(ask_id);
        loop {
            // biased: the verdict wins over a concurrent EOF. A client that
            // answers and closes immediately makes both the oneshot and
            // next_frame ready; without bias tokio picks at random and can
            // drop the delivered answer for a "client closed" error.
            tokio::select! {
                biased;
                resp = &mut response => {
                    return resp.map(Some).map_err(|_| {
                        ProtocolError::new(
                            ErrorCategory::Unavailable,
                            "permission waiter dropped",
                            false,
                        )
                    });
                }
                frame = io.next_frame() => match frame {
                    Some(InboundFrame::Response(resp)) => {
                        if !io.deliver_response(resp) {
                            tracing::warn!("response matched no pending permission ask");
                        }
                    }
                    Some(InboundFrame::Notification(notif))
                        if notif.method == "session/cancel" =>
                    {
                        self.runner.abort();
                        self.adapter.handle_notification(&notif).await;
                        self.cancel_requested.store(true, Ordering::Relaxed);
                        io.forget_response(ask_id);
                        return Ok(None);
                    }
                    Some(InboundFrame::Unparseable(line)) => {
                        tracing::warn!(bytes = line.len(), "inbound frame did not parse");
                        let resp = AcpResponse::err(
                            AcpRequestId::Null,
                            AcpErrorCode::ParseError,
                            "frame did not parse as a request, response, or notification",
                        );
                        self.send_response(io, &resp).await?;
                    }
                    Some(_) => {
                        return Err(ProtocolError::new(
                            ErrorCategory::InvalidFrame,
                            "unexpected frame while a permission ask is pending",
                            false,
                        ));
                    }
                    None => {
                        return Err(ProtocolError::new(
                            ErrorCategory::Unavailable,
                            "client closed mid-permission",
                            false,
                        ));
                    }
                },
            }
        }
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

    /// Forward one engine turn event as a session/update notification. Kinds
    /// the base protocol has no standard counterpart for (compaction boundary,
    /// summary, meta user, permission decision) ride the acpx/context/* side
    /// channel — that mapping lands with the acpx serve integration; a
    /// pure-ACP client ignores it anyway, so the base stream here is complete
    /// for the first cut.
    async fn push_turn_event(
        &mut self,
        io: &mut AcpDuplexPump,
        ev: &SessionLogEntry,
    ) -> Result<(), ProtocolError> {
        // When the live stream carried the turn text as deltas, the durable
        // Assistant and Reasoning events would repeat it as a whole message;
        // suppress them so the client does not render the turn twice. Other
        // durable events (tool calls, results, run completion) still push.
        if self.streamed_text.load(Ordering::Relaxed)
            && matches!(
                ev.event,
                SessionEvent::AssistantMessage { .. } | SessionEvent::Reasoning { .. }
            )
        {
            return Ok(());
        }
        if let Some(update) = map_session_update(&ev.event) {
            let params = serde_json::to_value(&update).expect("session update serialize");
            let notif = AcpNotification::new("session/update", params);
            self.send_typed(io, &notif).await?;
        }
        Ok(())
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
            let params = serde_json::to_value(&update).expect("stream update serialize");
            let notif = AcpNotification::new("session/update", params);
            self.send_typed(io, &notif).await?;
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

/// The plain text of a prompt, or an error message naming the first non-text
/// block the agent does not accept. The agent declared promptCapabilities
/// false for image, audio, and embedded context at initialize, so a block of
/// any of those kinds is rejected here rather than silently dropped.
fn prompt_text(prompt: Vec<ContentBlock>) -> Result<String, String> {
    let mut text = String::new();
    for block in prompt {
        match block {
            ContentBlock::Text { text: t } => text.push_str(&t),
            ContentBlock::Image { .. } => {
                return Err(
                    "image content is not supported (agent declared promptCapabilities.image false)"
                        .into(),
                );
            }
            ContentBlock::ResourceLink { uri, .. } => {
                return Err(format!(
                    "resource_link attachments are not supported: {uri}"
                ));
            }
            _ => return Err("unsupported content block".into()),
        }
    }
    Ok(text)
}

/// The next live delta, or pending forever when no stream is installed. A
/// select branch that never resolves keeps the run future and the frame
/// reader as the only live branches, so the absence of a stream costs
/// nothing.
async fn next_delta(live: &mut Option<AcpLiveStream>) -> Option<ResponseStreamEvent> {
    match live {
        Some(stream) => stream.recv().await,
        None => pending().await,
    }
}

/// The current wall clock as milliseconds since the Unix epoch.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
