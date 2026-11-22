//! Mid-run permission requests and verdict auditing, delegated to the
//! consent policy. The router owns the frontend frame exchange; the kernel
//! owns the plan, the install, and the audit.

use houyicoder_api::sandbox::BoundaryGrant;
use houyicoder_context::SandboxError;
use houyicoder_core::agent::{
    ApprovalDecision as EngineApprovalDecision, ApprovalRequest as EngineApprovalRequest,
};
use houyicoder_permission::StoreError;
use houyicoder_protocol::envelope::{
    ClientResponsePayload, ServerFrame, ServerRequestEnvelope, ServerRequestPayload,
};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::permission::AskReason;
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalScope, DelegationSource};

use crate::consent::{CardFence, ConsentInstall, ConsentPolicy, PlannedAsk, RawConsentChoice};
use crate::protocol_adapter::{build_approval_request, parse_approval_decision};

use super::{Server, frame_carrier::FrameCarrier};

#[cfg(test)]
use crate::consent::{disclose_directory_grants, format_skill_script_detail};
#[cfg(test)]
use houyicoder_protocol::envelope::ClientFrame;

mod response_wait;

impl Server {
    /// Build a consent policy from this server's gate, sandbox session, audit
    /// log, skill registry, and session id.
    pub(crate) fn consent(&self) -> ConsentPolicy {
        // The runner's sandbox session is the fence the tools execute under, so
        // it wins when both handles exist; a connection that attached its own
        // handle (an embedder or a test) is the fallback for a runner built
        // without one.
        ConsentPolicy::new(
            self.gate.clone(),
            self.runner
                .sandbox_session()
                .or_else(|| self.sandbox_session.clone()),
            self.runner.store(),
            self.runner.skill_registry().cloned(),
            self.session,
        )
    }

    /// Drive one approval request to a human answer: send the ask frame the
    /// policy planned, read the matching answer, settle, and return.
    pub(super) async fn handle_approval(
        &mut self,
        io: &mut FrameCarrier,
        approval: &EngineApprovalRequest,
        delegation: Option<DelegationSource>,
    ) -> Result<EngineApprovalDecision, ProtocolError> {
        let planned = self.consent().plan_ask(approval, delegation);
        let ask_id = self.mint_req_id();
        let ask = ServerRequestEnvelope::new(
            ask_id,
            ServerRequestPayload::Permission(build_approval_request(
                approval,
                planned.reason.as_ref(),
                planned.delegation.as_ref(),
                planned.fence,
                &planned.directory_grants,
            )),
        );
        Self::send_typed(io, &ServerFrame::Request(ask)).await?;
        self.consent().record_card(approval).await;
        let resp = match self.read_permission_response(io, ask_id).await? {
            Some(r) => r,
            None => {
                return Ok(EngineApprovalDecision {
                    call_id: approval.call_id.clone(),
                    approved: false,
                    updated_input: None,
                });
            }
        };
        let decision = match resp.payload {
            ClientResponsePayload::Permission(d) => d,
            _ => {
                return Err(ProtocolError::new(
                    ErrorCategory::InvalidFrame,
                    "expected a permission reverse response",
                    false,
                ));
            }
        };
        let choice = RawConsentChoice {
            approved: decision.approved,
            scope: decision.scope,
            updated_input: decision.updated_input.clone(),
        };
        let settled = self.consent().settle_answer(&planned, choice).await?;
        for notice in &settled.notices {
            self.notify_user(notice.clone());
        }
        if let Some(host) = &self.host {
            host.store()
                .advance_pending(self.session, settled.wire_decision.clone());
        }
        Ok(parse_approval_decision(settled.wire_decision))
    }

    /// Settle one answered ask against a policy plan and present the install
    /// notices. Preserves the legacy entry the approval test suite drives.
    pub(super) async fn settle_answer(
        &self,
        approval: &EngineApprovalRequest,
        decision: &ApprovalDecision,
        reason: Option<&AskReason>,
        grants: &[BoundaryGrant],
        is_entitlement: bool,
    ) -> Result<ApprovalDecision, ProtocolError> {
        let policy = self.consent();
        let grant = policy.consent_plan(&approval.tool_name, &approval.input, reason, grants);
        let planned = PlannedAsk {
            call_id: approval.call_id.clone(),
            tool_name: approval.tool_name.clone(),
            input: approval.input.clone(),
            is_entitlement,
            reason: reason.cloned(),
            delegation: None,
            directory_grants: grants.to_vec(),
            grant,
            options: Vec::new(),
            fence: policy.card_fence(),
        };
        let choice = RawConsentChoice {
            approved: decision.approved,
            scope: decision.scope,
            updated_input: decision.updated_input.clone(),
        };
        let settled = policy.settle_answer(&planned, choice).await?;
        for notice in &settled.notices {
            self.notify_user(notice.clone());
        }
        Ok(settled.wire_decision)
    }

    pub(crate) fn card_fence(&self) -> CardFence {
        self.consent().card_fence()
    }

    pub(crate) fn approval_directory_grants(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Vec<BoundaryGrant> {
        self.consent().approval_directory_grants(tool_name, input)
    }

    pub(crate) fn route_consent(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        choice: ApprovalScope,
        reason: Option<&AskReason>,
    ) -> Result<ConsentInstall, SandboxError> {
        self.consent()
            .route_consent(tool_name, input, choice, reason)
    }

    pub(crate) fn apply_consent_directory(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        lifetime: ApprovalScope,
    ) -> Result<ConsentInstall, SandboxError> {
        self.consent()
            .apply_consent_directory(tool_name, input, lifetime)
    }

    pub(crate) fn apply_consent_rule(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        reason: Option<&AskReason>,
        grants: &[BoundaryGrant],
        choice: ApprovalScope,
    ) -> Result<(), StoreError> {
        self.consent()
            .apply_consent_rule(tool_name, input, reason, grants, choice)
    }

    fn install_directory_grants(
        &self,
        grants: &[BoundaryGrant],
        lifetime: ApprovalScope,
    ) -> Result<Option<StoreError>, SandboxError> {
        self.consent().install_directory_grants(grants, lifetime)
    }

    pub(crate) fn augment_skill_script_reason(
        &self,
        reason: &mut AskReason,
        tool_name: &str,
        input: &serde_json::Value,
    ) {
        self.consent()
            .augment_skill_script_reason(reason, tool_name, input);
    }
}

#[cfg(test)]
mod tests;
