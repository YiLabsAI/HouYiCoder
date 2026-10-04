//! Mid-run permission requests, verdict auditing, and scoped consent
//! capabilities applied before execution resumes.

use houyicoder_api::sandbox::{
    BoundaryAccess, BoundaryGrant, Containment, SandboxSession, boundary_grants_for,
};
use houyicoder_api::skill::SkillScriptRef;
use houyicoder_context::{EventId, PermissionVerdict, SandboxError, SessionEvent, SessionLogEntry};
use houyicoder_core::agent::{
    ApprovalDecision as EngineApprovalDecision, ApprovalRequest as EngineApprovalRequest,
};
use houyicoder_permission::{AskReason, AskSource, Decision, Scope, ToolRequest};
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponsePayload, ServerFrame, ServerRequestEnvelope, ServerRequestPayload,
};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::extension::ENTITLEMENT_TOOL;
use houyicoder_protocol::frontend::run::{ApprovalDecision, DelegationSource};

use crate::composition::ContainmentAdapter;
use crate::protocol_adapter::{build_approval_request, parse_approval_decision};

use super::{Server, frame_carrier::FrameCarrier, now_millis};

impl Server {
    /// Drive one approval request to a human answer. Reconstructs the Ask
    /// reason the gate produced by re-running the ladder with the request the
    /// engine surfaced (the gate already decided Ask to get here, so the
    /// re-decide is a display-only reconstruction; no state mutates between the
    /// two calls except an intervening /mode or rule change, which is an
    /// accepted race (the state can change between the check and the use). Sends the wire ask, reads the
    /// matching reverse response, records the durable verdict audit, applies
    /// a scoped consent rule on yes-always, advances the parked-turn cursor,
    /// and returns the engine decision the runner resumes with.
    pub(super) async fn handle_approval(
        &mut self,
        io: &mut FrameCarrier,
        approval: &EngineApprovalRequest,
        delegation: Option<DelegationSource>,
    ) -> Result<EngineApprovalDecision, ProtocolError> {
        let is_entitlement = approval.tool_name == ENTITLEMENT_TOOL;
        let mut reason = if is_entitlement {
            // The entitlement ask is not a gate decision — reconstructing a
            // ladder reason for a host-generated tool would mislead the card.
            // The deny-log scan is the reason.
            None
        } else {
            self.reconstruct_reason(&approval.tool_name, &approval.input)
        };
        let directory_grants = self.approval_directory_grants(&approval.tool_name, &approval.input);
        if let Some(ref mut current) = reason {
            self.augment_skill_script_reason(current, &approval.tool_name, &approval.input);
        }
        disclose_directory_grants(&mut reason, &directory_grants);

        let ask_id = self.mint_req_id();
        let ask = ServerRequestEnvelope::new(
            ask_id,
            ServerRequestPayload::Permission(build_approval_request(
                approval,
                reason.as_ref(),
                delegation.as_ref(),
            )),
        );
        self.send_typed(io, &ServerFrame::Request(ask)).await?;
        // Read the matching reverse response. Loop, not a single read, so a
        // non-matching frame mid-ask does not fatal: a reconnecting client's
        // first status tick, a racing poll, a session/cancel, or a mode-cycle
        // all land here while the ask is in flight. The prior single read +
        // fatal arm returned InvalidFrame and closed the connection, so the run
        // never resumed (the AskUserQuestion deadlock). Aligns with the mid-run
        // and mid-resume selects: dispatch session/* notifications + mode-cycle
        // requests, drop the rest, fatal only on client close.
        let resp = loop {
            let frame = match io.next_frame().await {
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
                    // Esc: abort + return a deny so the serve loop resumes the
                    // now-cancelled run instead of hanging on a response the
                    // client will not send.
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
            // A non-matching frame (a Request, a mismatched Response, or an
            // unparseable frame) is dropped so the ask stays open. Surface it
            // so a protocol-incompatible client does not look stuck with no
            // clue (the old fatal arm at least logged the InvalidFrame).
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
        // Record the durable PermissionDecision audit event before resume
        // applies the decision. The engine resume path appends only the
        // ToolResult, never the verdict, so the verdict trail would be lost
        // over the wire without this append. The scope the client chose rides
        // the wire decision; the tool name + call_id come from the ask the
        // server sent.
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
                scope: decision.scope.clone(),
            },
        };
        if let Err(e) = self.runner.store().append(audit).await {
            // Best-effort audit: a failed append does not fail the run (the
            // verdict still reaches the engine via resume), only the durable
            // trail.
            tracing::warn!("permission-decision audit append failed: {e}");
        }
        if decision.approved && !is_entitlement {
            // Entitlement consent skips the rule/directory paths: the
            // grant-store write in the engine's apply path IS the
            // persistence. Persisting a rule for the host-generated tool would
            // pollute the store with a rule nothing reads.
            self.install_approved_consent(approval, &decision, reason.as_ref(), &directory_grants)?;
        }
        // Move this ask from remaining into decided so a mid-batch disconnect
        // re-emits only the tail and the runner resumes with the full decided
        // set.
        if let Some(host) = &self.host {
            host.store().advance_pending(self.session, decision.clone());
        }
        Ok(parse_approval_decision(decision))
    }

    /// Re-run the ladder to reconstruct the Ask reason the gate attached when
    /// it surfaced this approval, so the consent router can decide WHICH
    /// durable authorization (directory grant vs rule) to apply. The gate
    /// already decided Ask to get here; this re-decide is best-effort: state
    /// CAN shift between the original ask and this call — a prior approval in
    /// the same batch may have granted a directory to the fence, a /mode may
    /// have cycled, a rule may have been added — and a shift can return Allow
    /// (reason None) for a call the gate originally asked on. route_consent
    /// treats None as fail-closed (persist nothing) precisely because this
    /// re-decide is not authoritative. The carry-reason follow-up threads the
    /// original AskReason through the engine's ApprovalRequest so the consent
    /// path stops re-deciding and reads the reason the gate actually produced;
    /// that also removes the metrics double-count this re-decide incurs.
    pub(crate) fn reconstruct_reason(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Option<AskReason> {
        // is_destructive is unused by the ladder; is_read_only is hardcoded
        // false so a surfaced ask reproduces as the write-guarded case it
        // was (a read-only call passes the protected-path stage, so no
        // protected-path ask surfaces here; a rule, detection, or mode ask
        // still can). native_requires_approval is set true so a
        // mode-default ToolNative ask reproduces. A rule, safety, or
        // detection ask fires before the mode default regardless of the
        // flag, so every Ask path reconstructs.
        let req = ToolRequest {
            tool_name,
            input: Some(input),
            is_destructive: false,
            is_read_only: false,
            native_requires_approval: true,
        };
        match self.gate.decide(&req) {
            Decision::Ask(r) => Some(r),
            // Defensive: the engine only surfaces an approval when the gate
            // said Ask, so reaching Allow / Deny here means mode or rule state
            // shifted between the ask and this reconstruction. The ask still
            // goes out; the card renders a generic prompt.
            _ => None,
        }
    }

    /// When the ask is about a Bash command that runs a skill-directory
    /// script, replace the generic protected-path detail with the script's
    /// path so the approval card shows what would run. The detection is re-derived from the command because the gate's reason
    /// carries no skill context; no registry, no command field, or no skill
    /// script leaves the original detail untouched.
    fn augment_skill_script_reason(
        &self,
        reason: &mut AskReason,
        tool_name: &str,
        input: &serde_json::Value,
    ) {
        let is_shell = matches!(
            tool_name.to_ascii_lowercase().as_str(),
            "bash" | "sh" | "exec" | "shell"
        );
        if !is_shell {
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
        approval: &EngineApprovalRequest,
        decision: &ApprovalDecision,
        reason: Option<&AskReason>,
        grants: &[BoundaryGrant],
    ) -> Result<(), ProtocolError> {
        self.route_consent_with_grants(
            &approval.tool_name,
            &approval.input,
            &decision.scope,
            reason,
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

    /// Route an approved call to the directory capability required by its
    /// target and to any durable rule selected by the approval scope.
    pub(crate) fn route_consent(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        scope: &str,
        reason: Option<&AskReason>,
    ) -> Result<(), SandboxError> {
        let grants = self.approval_directory_grants(tool_name, input);
        self.route_consent_with_grants(tool_name, input, scope, reason, &grants)
    }

    /// Apply the exact directory grants disclosed on the approval card.
    pub(crate) fn route_consent_with_grants(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        scope: &str,
        reason: Option<&AskReason>,
        grants: &[BoundaryGrant],
    ) -> Result<(), SandboxError> {
        self.install_directory_grants(grants, scope)?;
        let is_path_bounds = reason.is_some_and(|current| current.validator == "path-bounds");
        let is_directory_capability =
            reason.is_some_and(|current| current.validator == "directory-capability");
        let is_system_safety =
            reason.is_some_and(|current| current.source == AskSource::SystemSafety);
        if !is_path_bounds
            && !is_directory_capability
            && !is_system_safety
            && reason.is_some()
            && scope == "always"
        {
            self.apply_consent_rule(tool_name, input);
        }
        Ok(())
    }

    /// Analyze directory capabilities required by one approved target.
    pub(crate) fn approval_directory_grants(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Vec<BoundaryGrant> {
        let Some(session) = &self.sandbox_session else {
            return Vec::new();
        };
        let bounds = ContainmentAdapter(session.clone());
        let Some(root) = bounds.boundary_root() else {
            return Vec::new();
        };
        boundary_grants_for(
            tool_name,
            Some(input),
            &root,
            &bounds.boundary_dirs(),
            &bounds.boundary_write_dirs(),
        )
    }

    /// Install directory grants derived from a structured tool target.
    pub(crate) fn apply_consent_directory(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        scope: &str,
    ) -> Result<(), SandboxError> {
        let grants = self.approval_directory_grants(tool_name, input);
        self.install_directory_grants(&grants, scope)
    }

    /// Install runtime grants before persisting them. A partial runtime update
    /// is rolled back when a later grant fails.
    fn install_directory_grants(
        &self,
        grants: &[BoundaryGrant],
        scope: &str,
    ) -> Result<(), SandboxError> {
        if grants.is_empty() {
            return Ok(());
        }
        let session = self.sandbox_session.as_ref().ok_or_else(|| {
            SandboxError::SandboxUnavailable("no sandbox session for directory grant".into())
        })?;
        let mut installed: Vec<&BoundaryGrant> = Vec::new();
        for grant in grants {
            if !grant.directory.is_dir() {
                rollback_directory_grants(session.as_ref(), &installed);
                return Err(SandboxError::NotFound(format!(
                    "authorized directory does not exist: {}",
                    grant.directory.display()
                )));
            }
            let path = grant.directory.to_string_lossy();
            let result = match grant.access {
                BoundaryAccess::ReadOnly => session.add_reading_dir(&path),
                BoundaryAccess::ReadWrite => session.add_working_dir(&path),
            };
            if let Err(error) = result {
                rollback_directory_grants(session.as_ref(), &installed);
                return Err(error);
            }
            installed.push(grant);
        }
        if scope == "always" {
            for grant in grants {
                match grant.access {
                    BoundaryAccess::ReadOnly => {
                        self.gate.add_read_directory(&grant.directory, Scope::Local)
                    }
                    BoundaryAccess::ReadWrite => {
                        self.gate.add_directory(&grant.directory, Scope::Local)
                    }
                }
            }
        }
        Ok(())
    }
}

fn rollback_directory_grants(session: &dyn SandboxSession, grants: &[&BoundaryGrant]) {
    for grant in grants {
        session.remove_working_dir(&grant.directory.to_string_lossy());
    }
}

/// Add the effective directory capability to the approval reason.
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
        validator: "directory-capability",
        detail: "external path requires a sandbox directory capability".into(),
        containment_note: None,
    });
    let first = &grants[0];
    let capability = match first.access {
        BoundaryAccess::ReadOnly => "read-only directory",
        BoundaryAccess::ReadWrite => "parent directory with read-write access",
    };
    let remaining = if grants.len() > 1 {
        format!(" and {} additional directories", grants.len() - 1)
    } else {
        String::new()
    };
    current.detail.push_str(&format!(
        "; approval also authorizes {capability} {}{remaining} for this session; choosing always persists it",
        first.directory.display()
    ));
}

/// Format the approval-card detail for a Bash command that runs one or more
/// skill-directory scripts: the skill name + relative script path. No first
/// line is shown — it is attacker-controlled text the card would frame as an
/// authoritative summary. Multiple scripts name the first and count the rest.
fn format_skill_script_detail(scripts: &[SkillScriptRef]) -> String {
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

#[cfg(test)]
#[path = "approval_tests.rs"]
mod tests;

// Every test in this module exercises the macOS sandbox consent chain,
// so the whole module compiles only on macOS. Gating the module keeps
// its imports from turning unused on other platforms.
#[cfg(all(test, target_os = "macos"))]
#[path = "consent_chain_tests.rs"]
mod consent_chain_tests;
