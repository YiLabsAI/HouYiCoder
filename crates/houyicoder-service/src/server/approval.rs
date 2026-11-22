//! Mid-run permission requests, verdict auditing, and scoped consent
//! capabilities applied before execution resumes.

use houyicoder_api::sandbox::BoundaryGrant;
use houyicoder_api::tool::is_shell_tool;
use houyicoder_context::{EventId, PermissionVerdict, SandboxError, SessionEvent, SessionLogEntry};
use houyicoder_core::agent::{
    ApprovalDecision as EngineApprovalDecision, ApprovalRequest as EngineApprovalRequest,
};
use houyicoder_permission::StoreError;
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponsePayload, ServerFrame, ServerRequestEnvelope, ServerRequestPayload,
};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::extension::ENTITLEMENT_TOOL;
use houyicoder_protocol::frontend::permission::AskReason;
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalScope, DelegationSource};

use crate::consent::{self, CardFence, ConsentInstall, DurableGrant, installed_scope};
use crate::protocol_adapter::{build_approval_request, parse_approval_decision};

use super::{Server, frame_carrier::FrameCarrier, now_millis};

impl Server {
    /// The two fence facts an approval card's option list reads, each taken
    /// from the axis it belongs to: a remembered command rests on bounded
    /// writes, and a remembered destination rests on the fence not refusing
    /// the egress.
    pub(crate) fn card_fence(&self) -> CardFence {
        consent::card_fence(&*self.gate)
    }

    /// Drive one approval request to a human answer. The cause travels with
    /// the request: the gate judged the call once, when the turn resolved, and
    /// the card shows that verdict. Re-deciding here would let a mode or rule
    /// change between the ask and the card describe a call the gate no longer
    /// asks about, and would judge the same call twice. Sends the ask frame,
    /// reads the matching answer frame, records the durable verdict audit,
    /// applies a scoped consent rule on yes-always, advances the parked-turn
    /// cursor, and returns the engine decision the runner resumes with.
    pub(super) async fn handle_approval(
        &mut self,
        io: &mut FrameCarrier,
        approval: &EngineApprovalRequest,
        delegation: Option<DelegationSource>,
    ) -> Result<EngineApprovalDecision, ProtocolError> {
        let is_entitlement = approval.tool_name == ENTITLEMENT_TOOL;
        let mut reason = approval.reason.clone();
        let directory_grants = self.approval_directory_grants(&approval.tool_name, &approval.input);
        if let Some(ref mut current) = reason {
            self.augment_skill_script_reason(current, &approval.tool_name, &approval.input);
        }
        disclose_directory_grants(&mut reason, &directory_grants);

        let ask_id = self.mint_req_id();
        let fence = self.card_fence();
        let ask = ServerRequestEnvelope::new(
            ask_id,
            ServerRequestPayload::Permission(build_approval_request(
                approval,
                reason.as_ref(),
                delegation.as_ref(),
                fence,
                &directory_grants,
            )),
        );
        Self::send_typed(io, &ServerFrame::Request(ask)).await?;
        // Recorded after the send: a card the connection never received was
        // never shown, so a failed send must leave no card in the audit.
        self.record_card(approval).await;
        // Read the matching reverse response, dispatching notifications and
        // deferring crossing client requests until the ask settles.
        let resp = loop {
            let frame = match io.next_live_frame().await? {
                Some(f) => f,
                None => {
                    return Err(ProtocolError::new(
                        ErrorCategory::Unavailable,
                        "client closed mid-permission",
                        false,
                    ));
                }
            };
            if let Ok(notif) = serde_json::from_str::<AcpNotification>(&frame) {
                let is_cancel = notif.method == "session/cancel";
                self.handle_session_notification(&notif);
                if is_cancel {
                    return Ok(EngineApprovalDecision {
                        call_id: approval.call_id.clone(),
                        approved: false,
                        updated_input: None,
                    });
                }
                continue;
            }
            if let Ok(ClientFrame::Response(r)) = serde_json::from_str::<ClientFrame>(&frame)
                && r.req_id == ask_id
            {
                break r;
            }
            if matches!(
                serde_json::from_str::<ClientFrame>(&frame),
                Ok(ClientFrame::Request(_))
            ) && io.defer_frame(frame.clone())
            {
                continue;
            }
            tracing::warn!(
                "ask-wait: dropped a non-matching frame while waiting for the permission response (first 80 chars): {}",
                frame.chars().take(80).collect::<String>()
            );
        };
        let decision = match resp.payload {
            ClientResponsePayload::Permission(d) => d,
            // non_exhaustive guard: a future reverse-response shape. The
            // reverse-request flow only asks Permission today.
            _ => {
                return Err(ProtocolError::new(
                    ErrorCategory::InvalidFrame,
                    "expected a permission reverse response",
                    false,
                ));
            }
        };
        // The settled answer is the decision to resume with: an answer whose
        // grant could not be installed comes back as a refusal of that one
        // call, so the connection and the turn carry on.
        let settled = self
            .settle_answer(
                approval,
                &decision,
                reason.as_ref(),
                &directory_grants,
                is_entitlement,
            )
            .await?;
        // Move this ask from remaining into decided so a mid-batch disconnect
        // re-emits only the tail and the runner resumes with the full decided
        // set.
        if let Some(host) = &self.host {
            host.store().advance_pending(self.session, settled.clone());
        }
        Ok(parse_approval_decision(settled))
    }

    /// Install what the answer keeps, then record the durable verdict at the
    /// lifetime the install committed. The client's choice can shorten the
    /// plan the ask derived but never lengthen it, and a save the store
    /// rejects shortens it further to the session, so the record states the
    /// grant that exists rather than the one that was asked for. A denial
    /// keeps nothing, and an entitlement names its own grant in the engine's
    /// apply path, which this router does not run.
    pub(super) async fn settle_answer(
        &self,
        approval: &EngineApprovalRequest,
        decision: &ApprovalDecision,
        reason: Option<&AskReason>,
        grants: &[BoundaryGrant],
        is_entitlement: bool,
    ) -> Result<ApprovalDecision, ProtocolError> {
        let grant = self.consent_plan(&approval.tool_name, &approval.input, reason, grants);
        let chosen = if !decision.approved {
            ApprovalScope::Once
        } else if is_entitlement {
            decision.scope
        } else {
            installed_scope(&grant, decision.scope)
        };
        // Entitlement consent skips the rule and directory paths: the
        // grant-store write in the engine's apply path IS the persistence, and
        // a rule for the host-generated tool would pollute the store with one
        // nothing reads.
        let committed = if decision.approved && !is_entitlement {
            match self.install_approved_consent(&grant, chosen, grants) {
                Ok(install) => {
                    if let Some(error) = install.save_error() {
                        self.notify_unsaved_grant(error);
                    }
                    install.lifetime()
                }
                Err(error) => {
                    // Nothing was installed, so the record must not claim a
                    // grant the session does not hold. The answer becomes a
                    // refusal of this one call rather than a broken
                    // connection: the user approved a path the fence could not
                    // take, and the model can retry somewhere it can write.
                    // Ending the session here is how one uninstallable
                    // directory used to close the whole client.
                    let refused = ApprovalDecision {
                        call_id: decision.call_id.clone(),
                        approved: false,
                        updated_input: None,
                        scope: ApprovalScope::Once,
                    };
                    self.record_verdict(approval, &refused, reason, ApprovalScope::Once)
                        .await;
                    self.notify_user(format!(
                        "The approved call did not run because the directory it needs could not be added. {error}"
                    ));
                    return Ok(refused);
                }
            }
        } else {
            chosen
        };
        self.record_verdict(approval, decision, reason, committed)
            .await;
        Ok(decision.clone())
    }

    /// Append the durable record that a card was shown. An ask the connection
    /// abandons keeps its record, which is what leaves it counted as a card
    /// with no verdict instead of vanishing. A re-emitted card records again
    /// and the fold counts one card per call id, so a reconnect neither
    /// double counts the card nor loses an ask whose first send never landed.
    pub(super) async fn record_card(&self, approval: &EngineApprovalRequest) {
        let audit = SessionLogEntry {
            id: EventId::new(),
            session: self.session,
            ts: now_millis(),
            prev_hash: None,
            event: SessionEvent::PermissionAsked {
                call_id: approval.call_id.clone(),
                tool: approval.tool_name.clone(),
            },
        };
        if let Err(error) = self.runner.store().append(audit).await {
            tracing::warn!("permission-asked audit append failed: {error}");
        }
    }

    /// Append the durable PermissionDecision for one answered ask. The engine
    /// resume path appends only the ToolResult, never the verdict, so without
    /// this append the session log would hold a result whose authorization
    /// was never recorded. The tool name and call id come from the ask the
    /// server sent.
    async fn record_verdict(
        &self,
        approval: &EngineApprovalRequest,
        decision: &ApprovalDecision,
        reason: Option<&AskReason>,
        lifetime: ApprovalScope,
    ) {
        let verdict = if decision.approved {
            PermissionVerdict::Approved
        } else {
            PermissionVerdict::Denied
        };
        let audit = SessionLogEntry {
            id: EventId::new(),
            session: self.session,
            ts: now_millis(),
            prev_hash: None,
            event: SessionEvent::PermissionDecision {
                call_id: approval.call_id.clone(),
                tool: approval.tool_name.clone(),
                verdict,
                scope: lifetime.as_str().to_string(),
                validator: reason.map(|r| r.validator.to_string()),
                reason: reason.map(|r| r.detail.clone()),
            },
        };
        if let Err(error) = self.runner.store().append(audit).await {
            // Best-effort audit: a failed append does not fail the run (the
            // verdict still reaches the engine via resume), only the durable
            // trail.
            tracing::warn!("permission-decision audit append failed: {error}");
        }
    }

    /// Tell the user a remembered authorization did not reach the store, so
    /// it lasts only until this session ends. Silence here would let the card
    /// promise a grant the next start does not have.
    fn notify_unsaved_grant(&self, error: &StoreError) {
        tracing::warn!("an approved authorization could not be saved: {error}");
        self.notify_user(format!(
            "The authorization could not be saved for this workspace ({error}); it stays in effect until this session ends."
        ));
    }

    /// When the ask is about a Bash command that runs a skill-directory
    /// script, replace the generic protected-path detail with the script's
    /// path so the approval card shows what would run. The skill is resolved
    /// from the command because an Ask reason names a validator, not a
    /// script; no registry, no command field, or no script leaves the
    /// original detail untouched.
    fn augment_skill_script_reason(
        &self,
        reason: &mut AskReason,
        tool_name: &str,
        input: &serde_json::Value,
    ) {
        if !is_shell_tool(tool_name) {
            return;
        }
        let Some(registry) = self.runner.skill_registry() else {
            return;
        };
        let Some(command) = input.get("command").and_then(|v| v.as_str()) else {
            return;
        };
        let scripts = registry.detect_run_scripts(command);
        if scripts.is_empty() {
            return;
        }
        reason.detail = format_skill_script_detail(&scripts);
    }

    fn install_approved_consent(
        &self,
        grant: &DurableGrant,
        lifetime: ApprovalScope,
        grants: &[BoundaryGrant],
    ) -> Result<ConsentInstall, ProtocolError> {
        consent::install_consent(
            self.sandbox_session.as_ref(),
            &*self.gate,
            grant,
            lifetime,
            grants,
        )
        .map_err(|error| {
            ProtocolError::new(
                ErrorCategory::Unavailable,
                format!("approved directory capability could not be installed: {error}"),
                false,
            )
        })
    }

    /// Analyze a call and derive the durable authorization an approved answer
    /// on it may keep. The card and the install both read this plan, so the
    /// lifetime the card states is the lifetime the install gives.
    pub(crate) fn consent_plan(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        reason: Option<&AskReason>,
        grants: &[BoundaryGrant],
    ) -> DurableGrant {
        consent::consent_plan(&*self.gate, tool_name, input, reason, grants)
    }

    /// Route an approved call to the directory capability required by its
    /// target and to whatever the chosen lifetime remembers.
    pub(crate) fn route_consent(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        choice: ApprovalScope,
        reason: Option<&AskReason>,
    ) -> Result<ConsentInstall, SandboxError> {
        consent::route_consent(
            self.sandbox_session.as_ref(),
            &*self.gate,
            tool_name,
            input,
            choice,
            reason,
        )
    }

    /// Apply whatever a remembered verdict authorizes, bounded by the lifetime
    /// the client chose.
    pub(crate) fn apply_consent_rule(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        reason: Option<&AskReason>,
        grants: &[BoundaryGrant],
        choice: ApprovalScope,
    ) -> Result<(), StoreError> {
        consent::apply_consent_rule(&*self.gate, tool_name, input, reason, grants, choice)
    }

    /// Install directory grants derived from a structured tool target, for
    /// exactly the lifetime the caller names.
    pub(crate) fn apply_consent_directory(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        lifetime: ApprovalScope,
    ) -> Result<ConsentInstall, SandboxError> {
        consent::apply_consent_directory(
            self.sandbox_session.as_ref(),
            &*self.gate,
            tool_name,
            input,
            lifetime,
        )
    }

    /// Analyze directory capabilities required by one approved target.
    pub(crate) fn approval_directory_grants(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Vec<BoundaryGrant> {
        consent::approval_directory_grants(self.sandbox_session.as_ref(), tool_name, input)
    }

    /// Install runtime grants before persisting them. A partial runtime update
    /// is rolled back when a later grant fails, restoring the access each
    /// directory had before this install.
    fn install_directory_grants(
        &self,
        grants: &[BoundaryGrant],
        lifetime: ApprovalScope,
    ) -> Result<Option<StoreError>, SandboxError> {
        consent::install_directory_grants(
            self.sandbox_session.as_ref(),
            &*self.gate,
            grants,
            lifetime,
        )
    }
}

mod reason;

pub(crate) use reason::disclose_directory_grants;
use reason::format_skill_script_detail;

#[cfg(test)]
mod tests;
