//! The mid-run permission reverse-request: the serve loop surfaced an
//! ApprovalRequest (the gate returned Ask), so the server pauses the run,
//! asks the human over the wire, records the verdict audit, applies any
//! yes-don't-ask-again consent, and returns the engine decision for resume.
//! Split from server.rs so that file stays under the file-size gate.

use houyicoder_context::{EventId, PermissionVerdict, TurnEvent, TurnEventKind};
use houyicoder_permission::{Decision, ToolRequest};
use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponsePayload, ServerFrame, ServerRequestEnvelope, ServerRequestPayload,
};
use houyicoder_protocol::wire::{WireError, WireErrorKind};

use crate::projection::{build_approval_request, parse_approval_decision};

use super::{Server, io::ServerIo, now_millis};

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
        io: &mut ServerIo,
        approval: &houyicoder_core::agent::ApprovalRequest,
    ) -> Result<houyicoder_core::agent::ApprovalDecision, WireError> {
        let mut reason = self.reconstruct_reason(&approval.tool_name, &approval.input);
        // Enrich the wire-display detail with the skill script path so the
        // card shows what would run. Only this path pays the detection IO;
        // resume_pending calls reconstruct_reason to route consent and
        // discards the detail, so it skips the augment.
        if let Some(ref mut r) = reason {
            self.augment_skill_script_reason(r, &approval.tool_name, &approval.input);
        }

        let ask_id = self.mint_req_id();
        let ask = ServerRequestEnvelope::new(
            ask_id,
            ServerRequestPayload::Permission(build_approval_request(approval, reason.as_ref())),
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
                    self.flush_pushed_count();
                    return Err(WireError::new(
                        WireErrorKind::Unavailable,
                        "client closed mid-permission",
                        false,
                    ));
                }
            };
            if let Ok(notif) =
                serde_json::from_str::<houyicoder_protocol::acp_wire::AcpNotification>(&frame)
            {
                let is_cancel = notif.method == "session/cancel";
                self.handle_session_notification(&notif);
                if is_cancel {
                    // Esc: abort + return a deny so the serve loop resumes the
                    // now-cancelled run instead of hanging on a response the
                    // client will not send.
                    return Ok(houyicoder_core::agent::ApprovalDecision {
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
                return Err(WireError::new(
                    WireErrorKind::InvalidFrame,
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
        let audit = TurnEvent {
            id: EventId::new(),
            session: self.session,
            ts: now_millis(),
            prev_hash: None,
            kind: TurnEventKind::PermissionDecision {
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
        // Route the consent by the ASK REASON, not the tool name: a path-bounds
        // approval grants the directory (apply_consent_directory); anything
        // else takes the rule path on scope "always". Shared with resume_pending
        // via route_consent so the two consent sites cannot drift.
        if decision.approved {
            self.route_consent(
                &approval.tool_name,
                &approval.input,
                &decision.scope,
                reason.as_ref(),
            );
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
    ) -> Option<houyicoder_permission::AskReason> {
        // is_destructive / is_read_only are unused by the ladder (no validator
        // reads them); native_requires_approval is set true so a mode-default
        // ToolNative ask reproduces. A rule, safety, or detection ask fires
        // before the mode default regardless of the flag, so every Ask path
        // reconstructs.
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
        reason: &mut houyicoder_permission::AskReason,
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

    /// Route an approval's consent by the ASK REASON, not the tool name: only a
    /// path-bounds ask grants a directory (apply_consent_directory); an ask
    /// with any OTHER reason takes the rule path (apply_consent_rule) on scope
    /// "always". A None reason — the re-decide could not reproduce why the gate
    /// asked — fails closed: nothing is persisted. Routing by tool name was
    /// wrong (any approved grep/glob got a directory grant even when the ask
    /// was a user-Ask rule), and routing a None reason to the rule path was
    /// worse: apply_consent_rule's non-bash terminal is a contentless
    /// tool-level Allow rule (matches the tool regardless of input, persisted
    /// at Project scope), so a batch that grants a directory then re-decides a
    /// nested path to Allow would silently install a permanent blanket grep
    /// allow that shadows all later path-bounds asks. None means "the consent
    /// authority does not know why this ask happened"; writing any durable
    /// authorization in that state would grant a permission the user never
    /// chose. Shared by handle_approval + resume_pending so the two consent
    /// sites cannot drift. The carry-reason follow-up threads the original
    /// AskReason through the engine's ApprovalRequest so this re-decide (and
    /// its None case) disappears from the consent path.
    pub(crate) fn route_consent(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        scope: &str,
        reason: Option<&houyicoder_permission::AskReason>,
    ) {
        let is_path_bounds = reason.is_some_and(|r| r.validator == "path-bounds");
        if is_path_bounds {
            self.apply_consent_directory(tool_name, input, scope);
        } else if reason.is_some() && scope == "always" {
            self.apply_consent_rule(tool_name, input);
        }
        // reason == None: fail closed. Persist nothing.
    }

    /// Route a path-bounds approval (grep/glob) to the two persistence layers:
    /// the kernel fence (additional_dirs, always — so the gate's re-check on
    /// resume passes instead of re-asking in a loop) and the durable store
    /// (only on scope "always", so the fence rehydrates the directory on
    /// restart). Mirrors apply_consent_rule for bash: same scope contract
    /// ("always" = persist, "once" = this session), reusing Scope rather than
    /// a new concept. The path extraction is the shared path_args_for_boundary
    /// (the gate uses the same helper for its pre-check ask) so the two layers
    /// cannot drift on which field is the path.
    pub(crate) fn apply_consent_directory(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        scope: &str,
    ) {
        let paths = houyicoder_api::sandbox::path_args_for_boundary(tool_name, Some(input));
        for p in paths {
            if let Some(session) = &self.sandbox_session
                && let Err(e) = session.add_working_dir(&p)
            {
                tracing::warn!(
                    "path-bounds consent: add_working_dir failed for {p}: {e}; the tool will still refuse the path"
                );
            }
            if scope == "always" {
                self.gate.add_directory(
                    std::path::Path::new(&p),
                    houyicoder_permission::Scope::Local,
                );
            }
        }
    }
}

/// Format the approval-card detail for a Bash command that runs one or more
/// skill-directory scripts: the skill name + relative script path. No first
/// line is shown — it is attacker-controlled text the card would frame as an
/// authoritative summary. Multiple scripts name the first and count the rest.
fn format_skill_script_detail(scripts: &[houyicoder_api::skill::SkillScriptRef]) -> String {
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
