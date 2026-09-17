//! Turn resolution: dispatch one turn's tool calls and compute the next step.
//!
//! Extracted from the runner module so the runner file stays under the size
//! gate and turn resolution is its own concern: classify every call the model
//! emitted, observe the redundant ones, run the PreToolUse hook gate over all
//! of them, split the survivors into the calls that run and the calls that
//! await a decision, execute the batches, record outcomes, and decide
//! RunAgain / FinalOutput / Interruption. The runner's drive loop calls this
//! once per model response.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use houyicoder_api::skill::GrantSubject;
use houyicoder_api::tool::Tool;
use houyicoder_context::SessionId;
use houyicoder_protocol::extension::ENTITLEMENT_TOOL;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::outcome_counts;
use super::step::{NextStep, extract_final_text};
use super::{ApprovalRequest, FallbackToolOutcome, RunError, Runner, obs_wire};

/// One tool call the model emitted this turn, with everything dispatch needs
/// to know about it. The tool's own gate on the input and its concurrency
/// class are read once, here: the hook gate, the approval partition, and the
/// execution batches all read the same record, so a call cannot be judged on
/// one classification and run under another.
pub(crate) struct ToolCallPlan {
    pub(crate) id: String,
    pub(crate) tool: Arc<dyn Tool>,
    pub(crate) input: Value,
    /// May this call run beside another one (see execute_partitioned).
    pub(crate) concurrency_safe: bool,
    /// Must the user approve this call before it runs.
    pub(crate) needs_approval: bool,
}

impl ToolCallPlan {
    fn new(id: String, tool: Arc<dyn Tool>, input: Value) -> Self {
        let concurrency_safe = tool.is_concurrency_safe();
        let needs_approval = tool.requires_approval_for(&input);
        Self {
            id,
            tool,
            input,
            concurrency_safe,
            needs_approval,
        }
    }
}

impl Runner {
    /// Resolve one turn: gate every call the model emitted, dispatch the ones
    /// that run in partition-by-safety batches (concurrency-safe parallel,
    /// mutating serial), collect the calls that await approval, compute
    /// NextStep. Results append in completion order, not model call order; tool
    /// errors become tool-result content (loop continues). A call the user
    /// must approve is NOT executed here — it becomes an Interruption the
    /// caller resolves via resume().
    pub(super) async fn resolve_turn(
        &self,
        session: SessionId,
        response: &CompletionResponse,
        token: &CancellationToken,
    ) -> Result<NextStep, RunError> {
        // The model's calls in order, each classified once by ToolCallPlan.
        let mut plans: Vec<ToolCallPlan> = Vec::new();
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
                    FallbackToolOutcome::UnknownTool {
                        name: name.clone(),
                        on_resume: false,
                    }
                    .to_json(),
                    0,
                )
                .await?;
                continue;
            };
            plans.push(ToolCallPlan::new(id.clone(), tool.clone(), input.clone()));
        }
        // Redundant-call observe + dedup reminder (harness self-evolution
        // observer): runs BEFORE run_pre_tool_use_gate so Deny/Feedback/
        // Ask-removed calls are still checked — the model DID emit a
        // duplicate; the block is downstream. Independent of the hook
        // registry (which early-returns when no user hooks are configured);
        // non-blocking, records + logs. Newly-flagged duplicates get a
        // MetaUser reminder so the next turn's model input carries a reuse
        // cue (instant feedback; the dream distills the same signal into
        // lessons — delayed feedback).
        let calls: Vec<(&str, &Value)> = plans
            .iter()
            .map(|plan| (plan.tool.name(), &plan.input))
            .collect();
        self.observe_redundancy(session, &calls).await;
        // Hook fire point: PreToolUse. Run the gate per tool before any execute;
        // Deny / Feedback / Ask remove the call + return a blocked
        // result the model sees losslessly, Allow / Observe / Trigger / Inject
        // keep it. Inject's input rewrite lands with the input-projection cut.
        // The gate reads every call the model emitted, a call awaiting approval
        // included: a rule that refuses a call outranks a decision on it, so
        // nobody is asked to approve what the rule already refused.
        let blocked = self.run_pre_tool_use_gate(session, &mut plans).await;
        // The survivors split: a call its own tool gates on the input becomes
        // an approval request the caller resolves via resume(); the rest run.
        let mut approvals: Vec<ApprovalRequest> = Vec::new();
        let mut exec: Vec<ToolCallPlan> = Vec::new();
        for plan in plans {
            if plan.needs_approval {
                let ToolCallPlan {
                    id, tool, input, ..
                } = plan;
                approvals.push(ApprovalRequest::new(id, tool.name().to_string(), input));
            } else {
                exec.push(plan);
            }
        }
        // Execute in partition-by-safety batches (concurrency-safe runs
        // concurrent, non-safe serial), PostToolUse firing after each call.
        // Each executed result is appended to the log as the call completes,
        // so the live delta renders per-tool progress, not a batch dump when
        // the slowest parallel call returns. Blocked results (Deny/Feedback/
        // Ask) had no execution, so they append after.
        let mut results = self.execute_partitioned(session, &exec, token).await?;
        for (id, output) in &blocked {
            self.append_tool_result(session, id.clone(), "", output.clone(), 0)
                .await?;
        }
        results.extend(blocked);
        self.fold_tool_outcomes(&results, &call_names);
        if let Some(skill) = self.active_skill()
            && let Some(req) = self.skill_registry.as_ref().and_then(|registry| {
                let source = super::skill_body::skill_source(&**registry, &skill)?;
                let subject = source.grant_subject(&skill);
                let origin = registry
                    .list_with_origin()
                    .into_iter()
                    .find(|snapshot| snapshot.descriptor.name == skill)
                    .map(|snapshot| snapshot.origin)
                    .unwrap_or_else(|| "unknown".to_string());
                scan_for_authorizable(&results, &call_names, &subject, &origin, &exec)
            })
        {
            // Append the raised ToolCall so the pending-approval scan on
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

    /// Fold one batch of tool outcomes into the session tallies: the tool
    /// counts behind /context and the observability log. Called by the turn
    /// that dispatched the calls and by the resume that released them, so the
    /// counts cover every call that ran, whichever path released it.
    pub(super) fn fold_tool_outcomes(
        &self,
        results: &[(String, Value)],
        call_names: &HashMap<String, String>,
    ) {
        let counts = outcome_counts::count_tool_outcomes(results);
        if let Ok(mut g) = self.usage.lock() {
            g.record_tool_batch(counts.calls, counts.ok, counts.err);
        }
        obs_wire::record_tool_outcomes(&self.observability, results, call_names);
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
    results: &[(String, Value)],
    call_names: &HashMap<String, String>,
    subject: &GrantSubject,
    origin: &str,
    exec: &[ToolCallPlan],
) -> Option<ApprovalRequest> {
    static RAISE_SEQ: AtomicU64 = AtomicU64::new(0);
    // Build a call_id → command map from the exec list so the approval
    // card can display the command that triggered the denial.
    let commands: HashMap<&str, &str> = exec
        .iter()
        .filter_map(|plan| {
            plan.input
                .get("command")
                .and_then(|v| v.as_str())
                .map(|cmd| (plan.id.as_str(), cmd))
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
                format!("entitlement-{seq}-{}", subject.skill),
                ENTITLEMENT_TOOL.to_string(),
                serde_json::json!({
                    "skill": subject.skill,
                    "origin": origin,
                    "grant_subject": subject.to_json(),
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
    use houyicoder_api::skill::{SkillFamily, SkillProvenance, SkillSource};

    fn subject(skill: &str) -> GrantSubject {
        SkillSource::new(SkillFamily::Houyi, SkillProvenance::UserHome).grant_subject(skill)
    }

    fn bash_names(ids: &[&str]) -> HashMap<String, String> {
        ids.iter()
            .map(|i| (i.to_string(), "bash".to_string()))
            .collect()
    }

    /// An empty exec list (no commands mapped).
    fn empty_exec() -> Vec<ToolCallPlan> {
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
            &subject("ego-browser"),
            "user",
            &empty_exec(),
        );
        let req = req.expect("bash result with services raises");
        assert_eq!(req.tool_name, ENTITLEMENT_TOOL);
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
            &subject("s"),
            "user",
            &empty_exec(),
        )
        .unwrap();
        let b = scan_for_authorizable(
            &results,
            &bash_names(&["call-1"]),
            &subject("s"),
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
            scan_for_authorizable(
                &results,
                &names,
                &subject("ego-browser"),
                "user",
                &empty_exec(),
            )
            .is_none()
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
                &subject("ego-browser"),
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
                &subject("ego-browser"),
                "user",
                &empty_exec()
            )
            .is_none()
        );
    }
}
