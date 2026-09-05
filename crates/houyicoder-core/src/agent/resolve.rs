//! Turn resolution: dispatch one turn's tool calls and compute the next step.
//!
//! Extracted from the runner module so the runner file stays under the size
//! gate and turn resolution is its own concern: collect executable +
//! approval-requiring calls, observe redundant calls, arbitrate PreToolUse
//! hooks, execute the partitioned batches, record outcomes, and decide
//! RunAgain / FinalOutput / Interruption. The runner's drive loop calls this
//! once per model response.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use houyicoder_api::tool::Tool;
use houyicoder_context::SessionId;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem};
use tokio_util::sync::CancellationToken;

use super::outcome_counts;
use super::step::{NextStep, extract_final_text};
use super::{ApprovalRequest, RunError, Runner, SyntheticToolOutcome, obs_wire};

impl Runner {
    /// Resolve one turn: dispatch non-approval tools in partition-by-safety
    /// batches (concurrency-safe parallel, mutating serial), collect
    /// approval-requiring calls, compute NextStep. Results append in
    /// completion order, not model call order; tool errors become
    /// tool-result content (loop continues). Approval-requiring tools are NOT
    /// executed — they become an Interruption the caller resolves via resume().
    pub(super) async fn resolve_turn(
        &self,
        session: SessionId,
        response: &CompletionResponse,
        token: &CancellationToken,
    ) -> Result<NextStep, RunError> {
        let mut approvals = Vec::new();
        // (call_id, tool, input, is_concurrency_safe) for executable calls,
        // kept in the model's call order.
        let mut exec: Vec<(String, Arc<dyn Tool>, serde_json::Value, bool)> = Vec::new();
        let mut call_names: HashMap<String, String> = HashMap::new();
        for item in &response.output {
            let OutputItem::ToolCall { id, name, input } = item else {
                continue;
            };
            call_names.insert(id.clone(), name.clone());
            let Some(tool) = self.tools.get(name) else {
                self.append_tool_result(
                    session,
                    id.clone(),
                    name,
                    SyntheticToolOutcome::UnknownTool {
                        name: name.clone(),
                        on_resume: false,
                    }
                    .to_json(),
                    0,
                )
                .await?;
                continue;
            };
            if tool.requires_approval_for(input) {
                approvals.push(ApprovalRequest::new(
                    id.clone(),
                    name.clone(),
                    input.clone(),
                ));
                continue;
            }
            exec.push((
                id.clone(),
                tool.clone(),
                input.clone(),
                tool.is_concurrency_safe(),
            ));
        }
        // Redundant-call observe + dedup reminder (harness self-evolution
        // observer): runs BEFORE arbitrate_pre_tool_use so Deny/Feedback/
        // Ask-removed calls are still checked — the model DID emit a
        // duplicate; the block is downstream. Independent of the hook
        // registry (which early-returns when no user hooks are configured);
        // non-blocking, records + logs. Newly-flagged duplicates get a
        // MetaUser reminder so the next turn's model input carries a reuse
        // cue (instant feedback; the dream distills the same signal into
        // lessons — delayed feedback).
        let calls: Vec<(&str, &serde_json::Value)> = exec
            .iter()
            .map(|(_, t, input, _)| (t.name(), input))
            .collect();
        self.observe_redundancy(session, &calls).await;
        // Hook fire point: PreToolUse. Arbitrate per tool before any execute;
        // Deny / Feedback / Ask remove the call + return a synthetic blocked
        // result the model sees losslessly, Allow / Observe / Trigger / Inject
        // keep it. Inject's input rewrite lands with the input-projection cut.
        let blocked = self.arbitrate_pre_tool_use(session, &mut exec).await;
        // Execute in partition-by-safety batches (concurrency-safe runs
        // concurrent, non-safe serial), PostToolUse firing after each call.
        // Each executed result is appended to the log as the call completes,
        // so the live delta renders per-tool progress, not a batch dump when
        // the slowest parallel call returns. Blocked results (Deny/Feedback/
        // Ask) are synthetic and have no execution, so they append after.
        let mut results = self.execute_partitioned(session, &exec, token).await?;
        for (id, output) in &blocked {
            self.append_tool_result(session, id.clone(), "", output.clone(), 0)
                .await?;
        }
        results.extend(blocked);
        // Count success/error (an {"error": ..} payload is an error) for the
        // /context tool tally under one lock.
        let counts = outcome_counts::count_tool_outcomes(&results);
        if let Ok(mut g) = self.usage.lock() {
            g.record_tool_batch(counts.calls, counts.ok, counts.err);
        }
        obs_wire::record_tool_outcomes(&self.observability, &results, &call_names);
        if let Some(skill) = self.active_skill()
            && let Some(req) = {
                let origin = self
                    .skill_registry
                    .as_ref()
                    .and_then(|r| super::skill_body::skill_origin(&**r, &skill))
                    .unwrap_or_else(|| "unknown".to_string());
                scan_for_authorizable(&results, &call_names, &skill, &origin, &exec)
            }
        {
            // Append the synthetic ToolCall so the pending-approval scan on
            // resume finds it (the decision routes by log call_id) and the
            // model sees a coherent ToolCall + ToolResult pair.
            self.append_tool_call(session, &req.call_id, &req.tool_name, req.input.clone())
                .await?;
            approvals.push(req);
        }
        if !approvals.is_empty() {
            return Ok(NextStep::Interruption(approvals));
        }
        if response.has_tool_calls() {
            return Ok(NextStep::RunAgain);
        }
        // No pending tools and no approval requests: the turn is final only if
        // the model emitted text. A turn with no Text and no ToolCalls (e.g.
        // only Reasoning, or empty) is "model said nothing usable" ⇒
        // run_again; max_turns is the backstop. Returning FinalOutput("")
        // here would silently end the run with an empty answer.
        match extract_final_text(&response.output) {
            Some(text) => Ok(NextStep::FinalOutput(text)),
            None => Ok(NextStep::RunAgain),
        }
    }
}

/// Scan executed bash results for authorizable_services (mach services
/// the sandbox blocked during a failed command). Only bash results are
/// considered — another tool echoing the field must not mint an
/// approval. Returns an entitlement approval request when a result
/// carries a non-empty set, with a per-raise unique call_id (the
/// answered-set keys on call_id; a repeated raise after a decline must
/// not collide with the first). The triggering command is included so
/// the approval card can show the user what was blocked, not just the
/// service name — the user needs the command to judge whether the
/// request is legitimate.
fn scan_for_authorizable(
    results: &[(String, serde_json::Value)],
    call_names: &HashMap<String, String>,
    skill: &str,
    origin: &str,
    exec: &[(String, Arc<dyn Tool>, serde_json::Value, bool)],
) -> Option<ApprovalRequest> {
    static RAISE_SEQ: AtomicU64 = AtomicU64::new(0);
    // Build a call_id → command map from the exec list so the approval
    // card can display the command that triggered the denial.
    let commands: HashMap<&str, &str> = exec
        .iter()
        .filter_map(|(id, _, input, _)| {
            input
                .get("command")
                .and_then(|v| v.as_str())
                .map(|cmd| (id.as_str(), cmd))
        })
        .collect();
    for (id, output) in results {
        let is_bash = call_names
            .get(id)
            .is_some_and(|n| n.eq_ignore_ascii_case("bash"));
        if !is_bash {
            continue;
        }
        if let Some(services) = output.get("authorizable_services")
            && services.is_array()
            && !services.as_array().unwrap().is_empty()
        {
            let seq = RAISE_SEQ.fetch_add(1, Ordering::Relaxed);
            let command = commands.get(id.as_str()).copied().unwrap_or("");
            return Some(ApprovalRequest::new(
                format!("entitlement-{seq}-{skill}"),
                houyicoder_protocol::extension::ENTITLEMENT_TOOL.to_string(),
                serde_json::json!({
                    "skill": skill,
                    "origin": origin,
                    "services": services,
                    "command": command,
                }),
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash_names(ids: &[&str]) -> HashMap<String, String> {
        ids.iter()
            .map(|i| (i.to_string(), "bash".to_string()))
            .collect()
    }

    /// An empty exec list (no commands mapped).
    fn empty_exec() -> Vec<(String, Arc<dyn Tool>, serde_json::Value, bool)> {
        Vec::new()
    }

    #[test]
    fn test_scan_finds_services() {
        let results = vec![
            ("call-1".into(), serde_json::json!({"success": true})),
            (
                "call-2".into(),
                serde_json::json!({"success": false, "authorizable_services": ["x.y.z"]}),
            ),
        ];
        let req = scan_for_authorizable(
            &results,
            &bash_names(&["call-1", "call-2"]),
            "ego-browser",
            "user",
            &empty_exec(),
        );
        let req = req.expect("bash result with services raises");
        assert_eq!(
            req.tool_name,
            houyicoder_protocol::extension::ENTITLEMENT_TOOL
        );
        assert!(req.call_id.starts_with("entitlement-"));
        assert!(req.call_id.ends_with("-ego-browser"));
        assert_eq!(req.input["skill"], "ego-browser");
        assert_eq!(req.input["origin"], "user");
    }

    #[test]
    fn test_scan_ids_unique() {
        let results = vec![(
            "call-1".into(),
            serde_json::json!({"authorizable_services": ["a.b"]}),
        )];
        let a = scan_for_authorizable(
            &results,
            &bash_names(&["call-1"]),
            "s",
            "user",
            &empty_exec(),
        )
        .unwrap();
        let b = scan_for_authorizable(
            &results,
            &bash_names(&["call-1"]),
            "s",
            "user",
            &empty_exec(),
        )
        .unwrap();
        assert_ne!(a.call_id, b.call_id, "repeated raises must not collide");
    }

    #[test]
    fn test_scan_ignores_non_bash() {
        let results = vec![(
            "call-1".into(),
            serde_json::json!({"authorizable_services": ["x.y.z"]}),
        )];
        let names = HashMap::from([("call-1".to_string(), "grep".to_string())]);
        assert!(
            scan_for_authorizable(&results, &names, "ego-browser", "user", &empty_exec()).is_none()
        );
    }

    #[test]
    fn test_scan_empty_services() {
        let results = vec![(
            "call-1".into(),
            serde_json::json!({"authorizable_services": []}),
        )];
        assert!(
            scan_for_authorizable(
                &results,
                &bash_names(&["call-1"]),
                "ego-browser",
                "user",
                &empty_exec()
            )
            .is_none()
        );
    }

    #[test]
    fn test_scan_no_field() {
        let results = vec![("call-1".into(), serde_json::json!({"success": false}))];
        assert!(
            scan_for_authorizable(
                &results,
                &bash_names(&["call-1"]),
                "ego-browser",
                "user",
                &empty_exec()
            )
            .is_none()
        );
    }
}
