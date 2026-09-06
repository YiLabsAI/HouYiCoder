//! Approval flow for Runner: resolve a caller's decisions onto the pending
//! tool calls, reconcile orphan calls on abort, and enumerate which calls
//! still await a decision. Extracted from the main impl so the approval
//! path and its call_id-uniqueness invariant live together.

use houyicoder_api::tool::ToolCtx;
use houyicoder_context::{SessionId, TurnEventKind};

use super::synthetic::{SyntheticToolOutcome, tool_error_json};
use super::{ApprovalDecision, ApprovalRequest, RunError, Runner};
use std::collections::HashSet;

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
    ) -> Result<Vec<ApprovalRequest>, RunError> {
        let mut remaining = Vec::new();
        for req in self.pending_approvals(session).await? {
            let Some(decision) = decisions.iter().find(|d| d.call_id == req.call_id) else {
                // No decision for this call: leave it pending so a later
                // resume can decide it. Re-surface it to the caller.
                remaining.push(req);
                continue;
            };
            if decision.approved {
                // Entitlement approval: write services to the grant store
                // instead of executing a tool. The services were discovered
                // by the deny-log scan after a failed bash command.
                if req.tool_name == houyicoder_protocol::extension::ENTITLEMENT_TOOL {
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
                    // execute_authorized honors a Yes (guarded tools proceed past
                    // Ask) and still blocks a tightened Deny at enforcement. A
                    // decision may carry an updated input (AskUserQuestion
                    // answers collected by the UI); use it so they reach the tool.
                    let input = decision.updated_input.clone().unwrap_or(req.input.clone());
                    let result = tool
                        .execute_authorized(
                            ToolCtx::new(req.call_id.as_str()).with_session(session),
                            input,
                        )
                        .await;
                    let output = match result {
                        Ok(v) => v,
                        Err(e) => tool_error_json(&e),
                    };
                    self.append_tool_result(
                        session,
                        req.call_id.clone(),
                        &req.tool_name,
                        output,
                        0,
                    )
                    .await?;
                } else {
                    self.append_tool_result(
                        session,
                        req.call_id.clone(),
                        &req.tool_name,
                        SyntheticToolOutcome::UnknownTool {
                            name: req.tool_name.clone(),
                            on_resume: true,
                        }
                        .to_json(),
                        0,
                    )
                    .await?;
                }
            } else {
                self.append_tool_result(
                    session,
                    req.call_id.clone(),
                    &req.tool_name,
                    SyntheticToolOutcome::Rejected.to_json(),
                    0,
                )
                .await?;
            }
        }
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
                SyntheticToolOutcome::Interrupted.to_json(),
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
            if let TurnEventKind::ToolResult { call_id, .. } = &e.kind {
                answered.insert(call_id.clone());
            }
        }
        let mut pending = Vec::new();
        for e in &events {
            if let TurnEventKind::ToolCall {
                call_id,
                tool,
                input,
            } = &e.kind
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
    store: Option<&houyicoder_api::skill::grant::SkillGrantStore>,
) -> serde_json::Value {
    let skill = input.get("skill").and_then(|v| v.as_str()).unwrap_or("");
    if skill.is_empty() {
        return serde_json::json!({ "error": "no skill named", "granted": [] });
    }
    let origin = input
        .get("origin")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
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
        match store.add_grants(skill, origin, services) {
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
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::process;

    #[test]
    fn test_entitlement_grants_services() {
        let dir = env::temp_dir().join(format!("houyi-entitlement-grant-{}", process::id()));
        let _ = fs::remove_dir_all(&dir).is_ok();
        fs::create_dir_all(&dir).expect("mkdir grant test");
        let path = dir.join("skill-grants.json");
        let store = houyicoder_api::skill::grant::SkillGrantStore::with_path(path);
        let input = serde_json::json!({
            "skill": "ego-browser",
            "origin": "user",
            "services": ["com.apple.trustd", "com.houyi.test.entitlement"],
        });
        let result = apply_entitlement(&input, Some(&store));
        assert_eq!(
            result["granted"],
            serde_json::json!(["com.houyi.test.entitlement"])
        );
        assert_eq!(
            result["message"],
            "Authorized 1 service(s) for ego-browser."
        );
        assert!(
            store
                .grant_for("ego-browser", "user")
                .contains(&"com.houyi.test.entitlement".to_string())
        );
        let _ = fs::remove_dir_all(&dir).is_ok();
    }

    #[test]
    fn test_entitlement_persist_failure() {
        let dir = env::temp_dir().join(format!("houyi-entitlement-error-{}", process::id()));
        let _cleanup = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir persistence test");
        let store = houyicoder_api::skill::grant::SkillGrantStore::with_path(dir.clone());
        let input = serde_json::json!({
            "skill": "ego-browser",
            "origin": "user",
            "services": ["com.houyi.test.entitlement"],
        });
        let result = apply_entitlement(&input, Some(&store));
        assert!(
            result["error"]
                .as_str()
                .is_some_and(|e| e.contains("persistence failed"))
        );
        assert!(result["granted"].as_array().is_some_and(Vec::is_empty));
        assert!(store.grant_for("ego-browser", "user").is_empty());
        let _cleanup = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_entitlement_no_store() {
        let input = serde_json::json!({ "skill": "x", "services": ["a.b.c"] });
        let result = apply_entitlement(&input, None);
        assert_eq!(result["error"], "grant store not wired");
    }

    #[test]
    fn test_entitlement_empty_skill_refused() {
        let dir = env::temp_dir().join(format!("houyi-entitlement-empty-{}", process::id()));
        let _ = fs::remove_dir_all(&dir).is_ok();
        fs::create_dir_all(&dir).expect("mkdir empty-skill test");
        let path = dir.join("skill-grants.json");
        let store = houyicoder_api::skill::grant::SkillGrantStore::with_path(path);
        let input = serde_json::json!({ "skill": "", "services": ["a.b.c"] });
        let result = apply_entitlement(&input, Some(&store));
        assert_eq!(result["error"], "no skill named");
        assert!(
            store.grant_for("", "unknown").is_empty(),
            "empty-name grant must not be written"
        );
        let _ = fs::remove_dir_all(&dir).is_ok();
    }
}
