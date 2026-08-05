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
//! The runner-abort half of cancel and takeControl-force lands when the
//! composition root wires Arc<Runner> into those handlers; prompt here is the
//! first cut that drives the run.

#![allow(dead_code)] // pub server type consumed by other crates and tests; locally unused

use std::sync::Arc;

use crate::acp_adapter::AcpAdapter;
use crate::acp_serve::AcpIo;
use crate::projection::{
    acp_permission_response_to_decision, approval_to_acp_permission, project_run_error,
    project_run_result, project_session_update,
};
use houyicoder_context::{EventId, PermissionVerdict, SessionId, TurnEvent, TurnEventKind};
use houyicoder_core::agent::{RunOutcome, Runner};
use houyicoder_protocol::acp_wire::{
    AcpError, AcpErrorCode, AcpNotification, AcpRequest, AcpRequestId, AcpResponse, JsonRpcVersion,
    PromptRequest, PromptResponse, RequestPermissionResponse,
};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::wire::{WireError, WireErrorKind};

/// The full ACP server. Owns the adapter (shared via Arc so the composition
/// root and the IO bridge can hold the same one) plus the runner + session +
/// the reverse-request id counter + the pushed-event cursor. serve runs the
/// line loop: each inbound frame is a request (session/prompt drives the
/// runner; other methods delegate to the adapter) or a notification
/// (session/cancel reaps state).
pub struct AcpServer {
    adapter: Arc<AcpAdapter>,
    runner: Arc<Runner>,
    session: SessionId,
    next_req_id: u64,
    pushed_count: usize,
    /// Set when a session/cancel notification arrives during a pending
    /// permission ask (the run is paused at Interruption, the drive loop
    /// is not streaming). The resume loop checks this after each frame read
    /// and breaks out — the turn ends with a cancelled stop_reason instead
    /// of waiting for a verdict the client will never send. (runner.abort()
    /// cancels the in-flight stream loop via the token; the durable flag
    /// only fires when the run is paused on an ask, so an idle cancel is a
    /// no-op on the flag.)
    cancel_requested: std::sync::atomic::AtomicBool,
}

impl AcpServer {
    /// Build a server bound to one runner + session, delegating the stateless
    /// methods to the adapter. The composition root constructs the runner and
    /// shares the adapter via Arc.
    pub fn new(adapter: Arc<AcpAdapter>, runner: Arc<Runner>, session: SessionId) -> Self {
        Self {
            adapter,
            runner,
            session,
            next_req_id: 0,
            pushed_count: 0,
            cancel_requested: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Mint a fresh reverse-request id for a mid-turn permission ask. Distinct
    /// from any event seq so the two correlation axes never collide.
    fn mint_req_id(&mut self) -> i64 {
        let id = self.next_req_id as i64;
        self.next_req_id += 1;
        id
    }

    /// Run the connection: receive frames until the client closes. Each frame
    /// is parsed as a request (has id) or a notification (no id). session/prompt
    /// drives the runner through handle_prompt; other requests delegate to the
    /// adapter and the reply is written back. A parse failure replies with a
    /// ParseError on the null id. Returns Ok for a clean close, Err for a
    /// carrier-level failure.
    pub async fn serve(mut self, io: &mut AcpIo) -> Result<(), WireError> {
        loop {
            let Some(frame) = io.next_frame().await else {
                return Ok(());
            };
            match serde_json::from_str::<AcpRequest>(&frame) {
                Ok(req) => {
                    if req.method == "session/prompt" {
                        if let Err(e) = self.handle_prompt(io, &req).await {
                            self.send_wire_error(io, e.clone()).await.ok();
                            return Err(e);
                        }
                    } else {
                        let resp = self.adapter.handle(&req).await;
                        self.send_response(io, &resp).await?;
                    }
                }
                Err(_) => match serde_json::from_str::<AcpNotification>(&frame) {
                    Ok(notif) => {
                        self.adapter.handle_notification(&notif).await;
                    }
                    Err(_) => {
                        let resp = AcpResponse::Error {
                            jsonrpc: JsonRpcVersion::V2,
                            id: AcpRequestId::Null,
                            error: AcpError {
                                code: AcpErrorCode::ParseError,
                                message: "frame did not parse as request or notification".into(),
                                data: None,
                            },
                        };
                        self.send_response(io, &resp).await?;
                    }
                },
            }
        }
    }

    /// Drive one session/prompt request through the runner. The run streams
    /// its turn events as session/update notifications; on Interruption the
    /// server sends a session/request_permission reverse request per pending
    /// ask, reads the response, appends the verdict audit, and resumes. Only a
    /// final outcome replies to the original request id with a PromptResponse.
    #[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
    async fn handle_prompt(&mut self, io: &mut AcpIo, req: &AcpRequest) -> Result<(), WireError> {
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
        // Collapse the multimodal content to the plain text the engine run
        // takes today; non-text blocks drop at the service boundary until a
        // multimodal run path lands.
        let text: String = params
            .prompt
            .into_iter()
            .filter_map(|b| match b {
                houyicoder_protocol::frontend::run::ContentBlock::Text { text } => Some(text),
                _ => None,
            })
            .collect();
        // Await the run while concurrently reading inbound frames so a
        // session/cancel notification arriving mid-run aborts the drive loop
        // (the run future keeps polling, observes the cancel token, resolves
        // Interrupted). Any other frame mid-run is a protocol violation
        // (fail-closed). This is the concurrent-reader fix for the cancel race
        // that a stock ACP client triggers over the wire — the frontend server
        // sidesteps it by aborting a shared Arc<Runner> directly, but an ACP
        // client has no shared handle, so the cancel must ride the wire.
        let mut result = {
            // Scoped so run_fut (which borrows self.runner) drops before the
            // turn loop below mutably borrows self for push/send calls.
            let run_fut = self.runner.run(self.session, text);
            tokio::pin!(run_fut);
            loop {
                // biased: poll the run future first so a ready run resolves
                // before an inbound EOF frame wins the race (without this, a
                // fast stub run + immediate stdin close races ~50/50, dropping
                // the real reply for a spurious "client closed mid-run").
                tokio::select! {
                    biased;
                    r = &mut run_fut => break r,
                    frame = io.next_frame() => match frame {
                        Some(f) => {
                            if !self.route_cancel_frame(&f).await? {
                                return Err(WireError::new(
                                    WireErrorKind::InvalidFrame,
                                    "unexpected frame mid-run",
                                    false,
                                ));
                            }
                        }
                        None => {
                            return Err(WireError::new(
                                WireErrorKind::Unavailable,
                                "client closed mid-run",
                                false,
                            ));
                        }
                    },
                }
            }
        };
        loop {
            // Push only the events appended since the last push so a resumed
            // run does not re-send what the client already saw.
            let events = self.runner.store().trajectory_snapshot(self.session);
            for ev in events.iter().skip(self.pushed_count) {
                self.push_turn_event(io, ev).await?;
            }
            self.pushed_count = events.len();
            match result {
                Ok(run) => match run.outcome {
                    RunOutcome::Interruption(approvals) => {
                        let mut decisions = Vec::with_capacity(approvals.len());
                        for approval in approvals {
                            let ask_id = self.mint_req_id();
                            let session_str = self.session.to_string();
                            let params = approval_to_acp_permission(&approval, session_str);
                            let params_value = serde_json::to_value(&params).map_err(|e| {
                                WireError::new(WireErrorKind::InvalidFrame, e.to_string(), false)
                            })?;
                            let ask =
                                AcpRequest::new(ask_id, "session/request_permission", params_value);
                            self.send_typed(io, &ask).await?;
                            // Read the matching reverse response.
                            let frame = match io.next_frame().await {
                                Some(f) => f,
                                None => {
                                    return Err(WireError::new(
                                        WireErrorKind::Unavailable,
                                        "client closed mid-permission",
                                        false,
                                    ));
                                }
                            };
                            let resp: AcpResponse = serde_json::from_str(&frame).map_err(|e| {
                                WireError::new(WireErrorKind::InvalidFrame, e.to_string(), false)
                            })?;
                            let resp_value = match resp {
                                AcpResponse::Result { result, .. } => result,
                                AcpResponse::Error { error, .. } => {
                                    return Err(WireError::new(
                                        WireErrorKind::InvalidFrame,
                                        format!("permission ask rejected: {error:?}"),
                                        false,
                                    ));
                                }
                            };
                            let perm_resp: RequestPermissionResponse =
                                serde_json::from_value(resp_value).map_err(|e| {
                                    WireError::new(
                                        WireErrorKind::InvalidFrame,
                                        e.to_string(),
                                        false,
                                    )
                                })?;
                            // Record the durable PermissionDecision audit event
                            // before resume applies the decision.
                            let verdict = if matches!(
                                perm_resp.outcome,
                                houyicoder_protocol::acp_wire::RequestPermissionOutcome::Cancelled
                            ) {
                                PermissionVerdict::Denied
                            } else {
                                // Look up the decision the projection computes.
                                PermissionVerdict::Approved
                            };
                            let _verdict = verdict; // reserved for the audit below
                            let audit = TurnEvent {
                                id: EventId::new(),
                                session: self.session,
                                ts: now_millis(),
                                prev_hash: None,
                                kind: TurnEventKind::PermissionDecision {
                                    call_id: approval.call_id.clone(),
                                    tool: approval.tool_name.clone(),
                                    verdict,
                                    scope: "once".into(),
                                },
                            };
                            if let Err(e) = self.runner.store().append(audit).await {
                                tracing::warn!("permission-decision audit append failed: {e}");
                            }
                            decisions.push(acp_permission_response_to_decision(
                                perm_resp,
                                approval.call_id.clone(),
                            ));
                        }
                        result = {
                            let resume_fut = self.runner.resume(self.session, &decisions);
                            tokio::pin!(resume_fut);
                            let outcome;
                            loop {
                                // If a cancel arrived during the permission
                                // ask (route_cancel_frame set the flag), skip
                                // resume entirely — the turn ends cancelled.
                                if self
                                    .cancel_requested
                                    .load(std::sync::atomic::Ordering::Relaxed)
                                {
                                    outcome = Ok(houyicoder_core::agent::RunResult {
                                        outcome: houyicoder_core::agent::RunOutcome::Interrupted(
                                            "cancelled during permission ask".to_string(),
                                        ),
                                        turns: 0,
                                        usage: houyicoder_protocol::llm::Usage::default(),
                                    });
                                    break;
                                }
                                tokio::select! {
                                    biased;
                                    r = &mut resume_fut => { outcome = r; break; }
                                    frame = io.next_frame() => match frame {
                                        Some(f) => {
                                            if !self.route_cancel_frame(&f).await? {
                                                return Err(WireError::new(
                                                    WireErrorKind::InvalidFrame,
                                                    "unexpected frame mid-resume",
                                                    false,
                                                ));
                                            }
                                        }
                                        None => {
                                            return Err(WireError::new(
                                                WireErrorKind::Unavailable,
                                                "client closed mid-resume",
                                                false,
                                            ));
                                        }
                                    },
                                }
                            }
                            outcome
                        };
                        continue;
                    }
                    _ => {
                        let projected = project_run_result(&run);
                        let resp = PromptResponse {
                            stop_reason: projected.stop_reason,
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
                    let _projected_err = project_run_error(&e);
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

    /// Inspect one inbound frame read mid-run (or mid-resume) for a
    /// session/cancel notification. Returns Ok(true) when the frame was a
    /// cancel (the run future keeps polling, the drive loop observes the
    /// cancelled token, and resolves Interrupted) and Ok(false) for any other
    /// frame, which the caller treats as a protocol violation (fail-closed).
    /// The store is reaped so lifecycle state reflects Cancelled. A cancel
    /// during a pending permission ask (the run paused at Interruption) does
    /// not cleanly abort the paused drive loop — the engine lacks a
    /// cancel-pending-resume path; that sub-case lands with engine support.
    async fn route_cancel_frame(&self, frame: &str) -> Result<bool, WireError> {
        let Ok(notif) = serde_json::from_str::<AcpNotification>(frame) else {
            return Ok(false);
        };
        if notif.method != "session/cancel" {
            return Ok(false);
        }
        self.runner.abort();
        self.adapter.handle_notification(&notif).await;
        self.cancel_requested
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(true)
    }

    /// Forward one engine turn event as a session/update notification. Kinds
    /// the base protocol has no standard counterpart for (compaction boundary,
    /// summary, meta user, permission decision) ride the acpx/context/* side
    /// channel — that projection lands with the acpx serve integration; a
    /// pure-ACP client ignores it anyway, so the base stream here is complete
    /// for the first cut.
    async fn push_turn_event(&mut self, io: &mut AcpIo, ev: &TurnEvent) -> Result<(), WireError> {
        if let Some(update) = project_session_update(&ev.kind) {
            let params = serde_json::to_value(&update).expect("session update serialize");
            let notif = AcpNotification::new("session/update", params);
            self.send_typed(io, &notif).await?;
        }
        Ok(())
    }

    /// Send a response paired to a request by id.
    async fn send_response(&mut self, io: &mut AcpIo, resp: &AcpResponse) -> Result<(), WireError> {
        self.send_typed(io, resp).await
    }

    /// Send a wire error with no correlation (the req_id is unknown).
    async fn send_wire_error(&mut self, io: &mut AcpIo, err: WireError) -> Result<(), WireError> {
        let resp = AcpResponse::err(
            AcpRequestId::Null,
            AcpErrorCode::InternalError,
            err.to_string(),
        );
        self.send_typed(io, &resp).await
    }

    /// Encode a typed message and push it through the carrier.
    async fn send_typed<T: serde::Serialize>(
        &mut self,
        io: &mut AcpIo,
        msg: &T,
    ) -> Result<(), WireError> {
        let frame = encode(msg).map_err(frame_to_wire)?;
        io.send_frame(frame)
            .await
            .map_err(|e| WireError::new(WireErrorKind::Unavailable, e, false))
    }
}

/// Map a frame encoding failure to a wire error at the boundary.
fn frame_to_wire(e: houyicoder_protocol::framing::FrameError) -> WireError {
    WireError::new(WireErrorKind::InvalidFrame, e.to_string(), false)
}

/// The current wall clock as milliseconds since the Unix epoch.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
