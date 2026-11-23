//! The permission reverse-request flow: send one card per pending ask, wait
//! for the client's verdict, and record the durable audit of both the
//! question and the decision. A cancel that arrives while a card is up ends
//! the turn without a verdict, and the card stays on the trail as an
//! outcome nobody stated.

use super::AcpServer;
use super::message::session_update;
use crate::acp::pump::AcpDuplexPump;
use crate::acp::pump::InboundFrame;
use crate::consent::{deny_delegated_ask, prepare_delegated_ask, publish_delegated_response};
use crate::protocol_adapter::acp_permission_response_to_decision;
use crate::protocol_adapter::approval_to_acp_permission;
use crate::protocol_adapter::parse_approval_decision;
use houyicoder_core::agent::ApprovalDecision;
use houyicoder_core::agent::ApprovalRequest;
use houyicoder_core::agent::multi_agent::bus_types::BusMessage;
use houyicoder_protocol::acp_wire::AcpErrorCode;
use houyicoder_protocol::acp_wire::AcpRequest;
use houyicoder_protocol::acp_wire::AcpRequestId;
use houyicoder_protocol::acp_wire::AcpResponse;
use houyicoder_protocol::acp_wire::RequestPermissionResponse;
use houyicoder_protocol::error::ErrorCategory;
use houyicoder_protocol::error::ProtocolError;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::run::DelegationSource;
use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};
use std::sync::atomic::Ordering;

impl AcpServer {
    /// Mint a fresh reverse-request id for a mid-turn permission ask. Distinct
    /// from any event seq so the two correlation axes never collide.
    pub(super) fn mint_req_id(&mut self) -> i64 {
        let id = self.next_req_id as i64;
        self.next_req_id += 1;
        id
    }

    /// Send one permission card per pending ask and collect the verdicts.
    /// Returns None when a session/cancel arrived while a card was up: the
    /// turn ends cancelled and resume is skipped.
    pub(super) async fn ask_permissions(
        &mut self,
        io: &mut AcpDuplexPump,
        approvals: Vec<ApprovalRequest>,
        delegation: Option<DelegationSource>,
    ) -> Result<Option<Vec<ApprovalDecision>>, ProtocolError> {
        let mut decisions = Vec::with_capacity(approvals.len());
        for approval in approvals {
            let planned = self.consent().plan_ask(&approval, delegation.clone());
            let ask_id = self.mint_req_id();
            let session_str = self.session.to_string();
            let params = approval_to_acp_permission(
                &approval,
                session_str,
                &planned.options,
                planned.reason.as_ref(),
                planned.delegation.as_ref(),
            );
            let params_value = serde_json::to_value(&params).map_err(|e| {
                ProtocolError::new(ErrorCategory::InvalidFrame, e.to_string(), false)
            })?;
            let ask = AcpRequest::new(ask_id, "session/request_permission", params_value);
            self.send_typed(io, &ask).await?;
            // The card is on the durable trail from the moment the client
            // received it: an ask abandoned or cancelled at the client records
            // no verdict, and the audit must still hold the question rather
            // than report that none was ever asked.
            self.consent().record_card(&approval).await;
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
            // A cancelled response carries no verdict: the card stays on the
            // trail as an outcome nobody stated, and the turn ends cancelled.
            let Some(choice) = acp_permission_response_to_decision(perm_resp, &planned) else {
                return Ok(None);
            };
            // The policy installs the grant the answer keeps (directory
            // widening, rule persistence with save-failure degradation) and
            // records the verdict audit with the real scope, validator, and
            // reason. Notices report a save that fell short or a directory the
            // fence could not take; they reach the client and the log, never a
            // silent gap between what the card promised and what the session
            // holds.
            let settled = self.consent().settle_answer(&planned, choice).await?;
            for notice in &settled.notices {
                tracing::warn!("consent notice: {notice}");
                let update =
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text {
                        text: notice.clone(),
                    }));
                let notification = session_update(self.session, &update)?;
                self.send_typed(io, &notification).await?;
            }
            decisions.push(parse_approval_decision(settled.wire_decision));
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

    /// Route one child permission ask through the ACP approval flow and
    /// publish the decision on the per-request response topic the child is
    /// awaiting. A cancelled card or a wire failure publishes a deny so the
    /// child resumes instead of hanging on a response that will not come.
    pub(super) async fn route_child_permission(
        &mut self,
        io: &mut AcpDuplexPump,
        msg: BusMessage,
    ) -> Result<(), ProtocolError> {
        let Some(ask) = prepare_delegated_ask(&self.runner, msg) else {
            return Ok(());
        };
        let result = self
            .ask_permissions(io, vec![ask.approval], Some(ask.delegation))
            .await;
        let bus = self.bus.as_ref();
        match result {
            Ok(Some(decisions)) => {
                let approved = decisions.first().is_some_and(|d| d.approved);
                let updated_input = decisions.first().and_then(|d| d.updated_input.clone());
                publish_delegated_response(
                    bus,
                    &ask.child_id,
                    &ask.call_id,
                    approved,
                    updated_input,
                );
                Ok(())
            }
            Ok(None) => {
                deny_delegated_ask(bus, &ask.child_id, &ask.call_id);
                Ok(())
            }
            Err(e) => {
                deny_delegated_ask(bus, &ask.child_id, &ask.call_id);
                Err(e)
            }
        }
    }
}
