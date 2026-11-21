//! The permission reverse-request flow: send one card per pending ask, wait
//! for the client's verdict, and record the durable audit of both the
//! question and the decision. A cancel that arrives while a card is up ends
//! the turn without a verdict, and the card stays on the trail as an
//! outcome nobody stated.

use super::AcpServer;
use super::now_millis;
use crate::acp::pump::AcpDuplexPump;
use crate::acp::pump::InboundFrame;
use crate::protocol_adapter::acp_permission_response_to_decision;
use crate::protocol_adapter::approval_to_acp_permission;
use houyicoder_context::EventId;
use houyicoder_context::SessionEvent;
use houyicoder_context::SessionLogEntry;
use houyicoder_core::agent::ApprovalDecision;
use houyicoder_core::agent::ApprovalRequest;
use houyicoder_protocol::acp_wire::AcpErrorCode;
use houyicoder_protocol::acp_wire::AcpRequest;
use houyicoder_protocol::acp_wire::AcpRequestId;
use houyicoder_protocol::acp_wire::AcpResponse;
use houyicoder_protocol::acp_wire::RequestPermissionResponse;
use houyicoder_protocol::error::ErrorCategory;
use houyicoder_protocol::error::ProtocolError;
use std::sync::atomic::Ordering;

impl AcpServer {
    /// Mint a fresh reverse-request id for a mid-turn permission ask. Distinct
    /// from any event seq so the two correlation axes never collide.
    pub(super) fn mint_req_id(&mut self) -> i64 {
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

    /// Send one permission card per pending ask and collect the verdicts.
    /// Returns None when a session/cancel arrived while a card was up: the
    /// turn ends cancelled and resume is skipped.
    pub(super) async fn ask_permissions(
        &mut self,
        io: &mut AcpDuplexPump,
        approvals: Vec<ApprovalRequest>,
    ) -> Result<Option<Vec<ApprovalDecision>>, ProtocolError> {
        let mut decisions = Vec::with_capacity(approvals.len());
        for approval in approvals {
            let ask_id = self.mint_req_id();
            let session_str = self.session.to_string();
            let params = approval_to_acp_permission(&approval, session_str);
            let params_value = serde_json::to_value(&params).map_err(|e| {
                ProtocolError::new(ErrorCategory::InvalidFrame, e.to_string(), false)
            })?;
            let ask = AcpRequest::new(ask_id, "session/request_permission", params_value);
            self.send_typed(io, &ask).await?;
            // The card is on the durable trail from the moment the client
            // received it: an ask abandoned or cancelled at the client records
            // no verdict, and the audit must still hold the question rather
            // than report that none was ever asked.
            self.record_asked(&approval).await;
            // A cancel that arrives while the card is up ends the turn
            // without a verdict: the remaining asks are not sent.
            let Some(resp) = self.await_permission(io, ask_id).await? else {
                return Ok(None);
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
                    ProtocolError::new(ErrorCategory::InvalidFrame, e.to_string(), false)
                })?;
            // Record the durable PermissionDecision audit event before resume
            // applies the decision. A cancelled response carries no verdict,
            // so the card stays on the trail as an outcome nobody stated.
            let reply = acp_permission_response_to_decision(perm_resp, approval.call_id.clone());
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
        Ok(Some(decisions))
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
}
