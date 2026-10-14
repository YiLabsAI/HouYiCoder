//! Approval flow for Runner: resolve a caller's decisions onto the pending
//! tool calls, reconcile orphan calls on abort, and enumerate which calls
//! still await a decision. Extracted from the main impl so the approval
//! path and its call_id-uniqueness invariant live together.

use houyicoder_api::skill::GrantSubject;
use houyicoder_api::skill::grant::SkillGrantStore;
use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_protocol::extension::ENTITLEMENT_TOOL;
use tokio_util::sync::CancellationToken;

use super::fallback::FallbackToolOutcome;
use super::{ApprovalDecision, ApprovalRequest, RunError, Runner};
use std::collections::{HashMap, HashSet};

impl Runner {
    /// Apply decisions to pending tool calls and return requests that still
    /// lack a decision. Unmatched requests remain pending for a later resume.
    ///
    /// Requires call_id to be unique within the session. Decisions route only
    /// by call_id, so a duplicate could authorize the wrong call. IDs are
    /// minted at the provider boundary; this function does not revalidate them.
    pub(crate) async fn apply_decisions(
        &self,
        session: SessionId,
        decisions: &[ApprovalDecision],
        token: &CancellationToken,
    ) -> Result<Vec<ApprovalRequest>, RunError> {
        let mut remaining = Vec::new();
        // Every released call's outcome, folded into the session tallies at
        // the end so a call that ran here counts like one the loop dispatched.
        let mut results: Vec<(String, serde_json::Value)> = Vec::new();
        let mut call_names: HashMap<String, String> = HashMap::new();
        let mut stopped = false;
        for req in self.pending_approvals(session).await? {
            call_names.insert(req.call_id.clone(), req.tool_name.clone());
            if stopped {
                // The stop landed while an approved call was running: the
                // calls behind it do not start. Each is recorded interrupted
                // so the session stays lossless and a later resume cannot run
                // a batch the user already stopped.
                let output = self
                    .record_interrupted(session, &req.call_id, &req.tool_name, &req.input)
                    .await?;
                results.push((req.call_id, output));
                continue;
            }
            let Some(decision) = decisions.iter().find(|d| d.call_id == req.call_id) else {
                // No decision for this call: leave it pending so a later
                // resume can decide it. Re-surface it to the caller.
                remaining.push(req);
                continue;
            };
            if decision.approved {
                // Entitlement approval: write services to the grant store
                // instead of executing a tool. Nothing runs, so it is not a
                // tool outcome and does not enter the tally. The services were
                // discovered by the deny-log scan after a failed bash command.
                if req.tool_name == ENTITLEMENT_TOOL {
                    let output = self.apply_entitlement_grant(&req.input);
                    self.append_tool_result(
                        session,
                        req.call_id.clone(),
                        &req.tool_name,
                        output,
                        0,
                    )
                    .await?;
                    continue;
                }
                if let Some(tool) = self.tools.get(&req.tool_name).cloned() {
                    // A decision may carry an updated input (the answers
                    // AskUserQuestion collected in the UI); use it so they
                    // reach the tool.
                    let input = decision.updated_input.clone().unwrap_or(req.input.clone());
                    let output = self
                        .execute_approved_call(session, req.call_id.as_str(), &tool, input, token)
                        .await?;
                    results.push((req.call_id.clone(), output));
                    stopped = token.is_cancelled();
                } else {
                    let output = FallbackToolOutcome::UnknownTool {
                        name: req.tool_name.clone(),
                        on_resume: true,
                    }
                    .to_json();
                    self.append_tool_result(
                        session,
                        req.call_id.clone(),
                        &req.tool_name,
                        output.clone(),
                        0,
                    )
                    .await?;
                    results.push((req.call_id, output));
                }
            } else {
                let output = FallbackToolOutcome::Rejected.to_json();
                self.append_tool_result(
                    session,
                    req.call_id.clone(),
                    &req.tool_name,
                    output.clone(),
                    0,
                )
                .await?;
                results.push((req.call_id, output));
            }
        }
        self.fold_tool_outcomes(&results, &call_names);
        Ok(remaining)
    }

    /// Write discovered mach services to the grant store so the next skill
    /// invocation includes them. Returns a JSON result the model sees.
    fn apply_entitlement_grant(&self, input: &serde_json::Value) -> serde_json::Value {
        apply_entitlement(input, self.skill_grants.as_deref())
    }

    /// Reconcile: append an interrupted-by-user result for every ToolCall with
    /// no matching ToolResult. Matches the reject branch so the session stays
    /// lossless after an abort (no orphan ToolCall without a result).
    pub(crate) async fn reconcile_tool_results(&self, session: SessionId) -> Result<(), RunError> {
        for req in self.pending_approvals(session).await? {
            self.append_tool_result(
                session,
                req.call_id.clone(),
                &req.tool_name,
                FallbackToolOutcome::Interrupted.to_json(),
                0,
            )
            .await?;
        }
        Ok(())
    }

    /// The approval requests pending for a session: ToolCall events whose
    /// call_id has no matching ToolResult yet. Used by resume() to know which
    /// calls to execute. (scans the replay; a real impl indexes this.)
    ///
    /// Precondition: call_id is unique across the session (minted at the
    /// provider boundary by unique_id_gen in openai_compat.rs). The answered
    /// set keys on call_id; a duplicate id would let one ToolResult mark
    /// every same-id call answered (silently dropping a pending call). The
    /// mint makes it unreachable; this function does not re-defend.
    async fn pending_approvals(
        &self,
        session: SessionId,
    ) -> Result<Vec<ApprovalRequest>, RunError> {
        let events = self.store.replay(session).await?;
        let mut answered = HashSet::new();
        for e in &events {
            if let SessionEvent::ToolResult { call_id, .. } = &e.event {
                answered.insert(call_id.clone());
            }
        }
        let mut pending = Vec::new();
        for e in &events {
            if let SessionEvent::ToolCall {
                call_id,
                tool,
                input,
            } = &e.event
                && !answered.contains(call_id)
            {
                pending.push(ApprovalRequest::new(
                    call_id.clone(),
                    tool.clone(),
                    input.clone(),
                ));
            }
        }
        Ok(pending)
    }
}

/// Merge discovered services into the grant store and return a JSON
/// result the model sees. Pure of runner state — takes the store by ref.
fn apply_entitlement(
    input: &serde_json::Value,
    store: Option<&SkillGrantStore>,
) -> serde_json::Value {
    let skill = input.get("skill").and_then(|v| v.as_str()).unwrap_or("");
    if skill.is_empty() {
        return serde_json::json!({ "error": "no skill named", "granted": [] });
    }
    let subject = input.get("grant_subject").and_then(GrantSubject::from_json);
    let Some(subject) = subject.filter(|subject| subject.skill == skill) else {
        return serde_json::json!({
            "error": "invalid grant subject",
            "skill": skill,
            "granted": [],
        });
    };
    let services: Vec<String> = input
        .get("services")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if let Some(store) = store {
        match store.add_grants(&subject, services) {
            Ok(granted) => {
                let count = granted.len();
                serde_json::json!({
                    "granted": granted,
                    "skill": skill,
                    "message": format!("Authorized {count} service(s) for {skill}."),
                })
            }
            Err(e) => serde_json::json!({
                "error": format!("grant persistence failed: {e}"),
                "skill": skill,
                "granted": [],
            }),
        }
    } else {
        serde_json::json!({ "error": "grant store not wired", "skill": skill })
    }
}

#[cfg(test)]
mod tests;
