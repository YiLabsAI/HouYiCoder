//! Protocol-agnostic consent kernel: the durable grant an approved answer
//! authorizes, the options the card offers, and the install that widens the
//! fence and persists the rule. ConsentPolicy bundles the gate, sandbox
//! session, audit log, skill registry, and session id, so the frontend and
//! ACP endpoints install the same grant for the same answer.

mod delegation;
mod grant_install;
mod policy;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use houyicoder_api::sandbox::{BoundaryAccess, BoundaryGrant, SandboxSession};
use houyicoder_api::session::SessionLog;
use houyicoder_api::skill::{SkillRegistry, SkillScriptRef};
use houyicoder_context::{EventId, PermissionVerdict, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_core::agent::ApprovalRequest as EngineApprovalRequest;
use houyicoder_permission::ModeGate;
use houyicoder_protocol::acp_wire::PermissionOption;
use houyicoder_protocol::error::ProtocolError;
use houyicoder_protocol::extension::ENTITLEMENT_TOOL;
use houyicoder_protocol::frontend::permission::{AskReason, AskSource};
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalScope, DelegationSource};

pub(crate) use delegation::{
    DelegatedAskReceiver, deny_delegated_ask, prepare_delegated_ask, publish_delegated_response,
};
pub(crate) use grant_install::ConsentInstall;
pub(crate) use policy::{
    ALLOW_ALWAYS, ALLOW_ONCE, CardFence, DurableGrant, approval_options, durable_grant,
    installed_scope,
};
#[cfg(test)]
pub(crate) use policy::{FENCED, UNFENCED, consent_rule_for};

/// The consent decisions for one session, bundled with the dependencies they
/// read so every protocol endpoint that asks the same question installs the
/// same grant. Constructed cheaply per ask from the host's fields.
pub(crate) struct ConsentPolicy {
    gate: Arc<dyn ModeGate>,
    sandbox_session: Option<Arc<dyn SandboxSession>>,
    store: Arc<dyn SessionLog>,
    skill_registry: Option<Arc<dyn SkillRegistry>>,
    session: SessionId,
}

impl ConsentPolicy {
    pub(crate) fn new(
        gate: Arc<dyn ModeGate>,
        sandbox_session: Option<Arc<dyn SandboxSession>>,
        store: Arc<dyn SessionLog>,
        skill_registry: Option<Arc<dyn SkillRegistry>>,
        session: SessionId,
    ) -> Self {
        Self {
            gate,
            sandbox_session,
            store,
            skill_registry,
            session,
        }
    }

    /// Freeze the card one turn will show: the directories a yes would widen,
    /// the reason with skill scripts and grants disclosed, the fence facts,
    /// the durable grant an always answer installs, and the options offered.
    pub(crate) fn plan_ask(
        &self,
        approval: &EngineApprovalRequest,
        delegation: Option<DelegationSource>,
    ) -> PlannedAsk {
        let is_entitlement = approval.tool_name == ENTITLEMENT_TOOL;
        let mut reason = approval.reason.clone();
        let directory_grants = self.approval_directory_grants(&approval.tool_name, &approval.input);
        if let Some(ref mut current) = reason {
            self.augment_skill_script_reason(current, &approval.tool_name, &approval.input);
        }
        disclose_directory_grants(&mut reason, &directory_grants);
        let fence = self.card_fence();
        let grant = durable_grant(
            &approval.tool_name,
            &approval.input,
            reason.as_ref(),
            fence,
            &directory_grants,
        );
        let options = approval_options(
            &approval.tool_name,
            &approval.input,
            reason.as_ref(),
            fence,
            &directory_grants,
        );
        PlannedAsk {
            call_id: approval.call_id.clone(),
            tool_name: approval.tool_name.clone(),
            input: approval.input.clone(),
            is_entitlement,
            reason,
            delegation,
            directory_grants,
            grant,
            options,
            fence,
        }
    }

    /// Install what the chosen answer keeps, then record the verdict at the
    /// lifetime the install committed. The choice can shorten the plan but
    /// never lengthen it, and a save the store rejects shortens it further to
    /// the session. A denial keeps nothing; an entitlement names its own
    /// grant in the engine's apply path. An install that cannot take the
    /// directory it needs refuses this one call and reports the reason.
    pub(crate) async fn settle_answer(
        &self,
        planned: &PlannedAsk,
        choice: RawConsentChoice,
    ) -> Result<SettledConsent, ProtocolError> {
        let chosen = if !choice.approved {
            ApprovalScope::Once
        } else if planned.is_entitlement {
            choice.scope
        } else {
            installed_scope(&planned.grant, choice.scope)
        };
        let wire_decision = ApprovalDecision {
            call_id: planned.call_id.clone(),
            approved: choice.approved,
            updated_input: choice.updated_input.clone(),
            scope: choice.scope,
        };
        let mut notices = Vec::new();
        let committed = if choice.approved && !planned.is_entitlement {
            match self.install_consent(&planned.grant, chosen, &planned.directory_grants) {
                Ok(install) => {
                    if let Some(error) = install.save_error() {
                        tracing::warn!("an approved authorization could not be saved: {error}");
                        notices.push(format!(
                            "The authorization could not be saved for this workspace ({error}); it stays in effect until this session ends."
                        ));
                    }
                    install.lifetime()
                }
                Err(error) => {
                    let refused = ApprovalDecision {
                        call_id: planned.call_id.clone(),
                        approved: false,
                        updated_input: None,
                        scope: ApprovalScope::Once,
                    };
                    self.record_verdict(planned, &refused, ApprovalScope::Once)
                        .await;
                    notices.push(format!(
                        "The approved call did not run because the directory it needs could not be added. {error}"
                    ));
                    return Ok(SettledConsent {
                        wire_decision: refused,
                        committed_lifetime: ApprovalScope::Once,
                        notices,
                    });
                }
            }
        } else {
            chosen
        };
        self.record_verdict(planned, &wire_decision, committed)
            .await;
        Ok(SettledConsent {
            wire_decision,
            committed_lifetime: committed,
            notices,
        })
    }

    /// Append the durable record that a card was shown.
    pub(crate) async fn record_card(&self, approval: &EngineApprovalRequest) {
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
        if let Err(error) = self.store.append(audit).await {
            tracing::warn!("permission-asked audit append failed: {error}");
        }
    }

    /// Append the durable PermissionDecision for one answered ask.
    async fn record_verdict(
        &self,
        planned: &PlannedAsk,
        decision: &ApprovalDecision,
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
                call_id: planned.call_id.clone(),
                tool: planned.tool_name.clone(),
                verdict,
                scope: lifetime.as_str().to_string(),
                validator: planned.reason.as_ref().map(|r| r.validator.to_string()),
                reason: planned.reason.as_ref().map(|r| r.detail.clone()),
            },
        };
        if let Err(error) = self.store.append(audit).await {
            tracing::warn!("permission-decision audit append failed: {error}");
        }
    }
}

/// One ask's frozen plan: what the card shows and what the install keeps.
pub(crate) struct PlannedAsk {
    pub(crate) call_id: String,
    pub(crate) tool_name: String,
    pub(crate) input: serde_json::Value,
    pub(crate) is_entitlement: bool,
    pub(crate) reason: Option<AskReason>,
    pub(crate) delegation: Option<DelegationSource>,
    pub(crate) directory_grants: Vec<BoundaryGrant>,
    pub(crate) grant: DurableGrant,
    pub(crate) options: Vec<PermissionOption>,
    pub(crate) fence: CardFence,
}

/// The user's answer, in protocol-agnostic form.
pub(crate) struct RawConsentChoice {
    pub(crate) approved: bool,
    pub(crate) scope: ApprovalScope,
    pub(crate) updated_input: Option<serde_json::Value>,
}

/// The settled outcome: the wire decision to resume with, the lifetime the
/// install committed, and notices the host must show the user.
pub(crate) struct SettledConsent {
    pub(crate) wire_decision: ApprovalDecision,
    pub(crate) committed_lifetime: ApprovalScope,
    pub(crate) notices: Vec<String>,
}

/// Add the effective directory capability to the approval reason, so a call
/// refused for another cause still says which outside directory a yes
/// authorizes. A path-bounds reason already names every grant; otherwise the
/// first directory is named and the rest counted.
pub(crate) fn disclose_directory_grants(reason: &mut Option<AskReason>, grants: &[BoundaryGrant]) {
    if grants.is_empty()
        || reason
            .as_ref()
            .is_some_and(|current| current.validator == "path-bounds")
    {
        return;
    }
    let current = reason.get_or_insert_with(|| AskReason {
        source: AskSource::Detection,
        validator: "directory-capability".to_string(),
        detail: "external path requires a sandbox directory capability".into(),
        containment_note: None,
    });
    let first = &grants[0];
    let capability = match first.access {
        BoundaryAccess::ReadOnly => "read-only directory",
        BoundaryAccess::ReadWrite => "parent directory with read-write access",
    };
    let remaining = match grants.len() {
        0 | 1 => String::new(),
        2 => " and 1 additional directory".to_string(),
        n => format!(" and {} additional directories", n - 1),
    };
    current.detail.push_str(&format!(
        ". Approving also grants {capability} {}{remaining}.",
        first.directory.display()
    ));
}

/// Format the approval-card detail for a Bash command that runs skill scripts.
pub(crate) fn format_skill_script_detail(scripts: &[SkillScriptRef]) -> String {
    match scripts.len() {
        0 => String::new(),
        1 => {
            let s = &scripts[0];
            format!("runs skill script {}/{}", s.skill_name, s.script_rel_path)
        }
        _ => {
            let s = &scripts[0];
            format!(
                "runs {} skill scripts, first is {}/{}",
                scripts.len(),
                s.skill_name,
                s.script_rel_path
            )
        }
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
