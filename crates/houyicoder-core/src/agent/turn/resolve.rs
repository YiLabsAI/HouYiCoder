//! Turn resolution: dispatch one turn's tool calls and compute the next step.
//!
//! Extracted from the runner module so the runner file stays under the size
//! gate and turn resolution is its own concern: classify every call the model
//! emitted, observe the redundant ones, run the PreToolUse hook gate over all
//! of them, split the survivors into the calls that run and the calls that
//! await a decision, execute the batches, record outcomes, and decide
//! RunAgain / FinalOutput / Interruption. The runner's drive loop calls this
//! once per model response.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use houyicoder_api::skill::GrantSubject;
use houyicoder_api::tool::{Tool, is_shell_tool, writes_files};
use houyicoder_context::SessionId;
use houyicoder_protocol::extension::{ENTITLEMENT_TOOL, ToolError};
use houyicoder_protocol::frontend::permission::AskReason;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::outcome_counts;
use crate::agent::skill_body::skill_source;
use crate::agent::step::{NextStep, extract_final_text};
use crate::agent::turn::fallback::tool_error_json;
use crate::agent::turn::withhold;
use crate::agent::{ApprovalRequest, FallbackToolOutcome, RunError, Runner, obs_wire};

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
    /// Why the user must approve this call, when they must.
    pub(crate) approval: Option<AskReason>,
    /// Required arguments this call does not supply. A non-empty list means
    /// the call cannot run: it is answered with the schema error instead of
    /// being executed or put to a human.
    pub(crate) invalid_arguments: Vec<String>,
}

/// The input a call runs, given the tool that runs it: the tool's own
/// resolution of the model's text, or that text when the tool resolves
/// nothing. One reading for every entry point, so a call planned now and the
/// same call resumed later cannot run different text.
pub(crate) fn resolved_input(tool: Option<&Arc<dyn Tool>>, input: &Value) -> Value {
    tool.and_then(|tool| tool.resolved_input(input))
        .unwrap_or_else(|| input.clone())
}

impl ToolCallPlan {
    pub(crate) fn new(id: String, tool: Arc<dyn Tool>, input: Value) -> Self {
        // The text the host runs may differ from the one the model wrote (a
        // shell translation). It resolves before the call is judged, so the
        // verdict, the approval card, and the execution all address the same
        // command; the transcript keeps the model's own call.
        let input = resolved_input(Some(&tool), &input);
        // The check rule reads a shell call against the edits before it in
        // emission order, and a parallel batch has no order: its members start
        // together. So the two families the rule names run in the serial batch,
        // in the order the model wrote them, whichever way their own tool
        // answers.
        let named_tool = tool.name();
        let concurrency_safe =
            tool.is_concurrency_safe() && !writes_files(named_tool) && !is_shell_tool(named_tool);
        // A call that fails its own required-argument check cannot run, so it
        // is neither executed nor put to a human. The model corrects itself on
        // the next turn from the error this turn returns; a reply cut at the
        // token cap produces exactly such calls, and interrupting a human for
        // one spends attention on a call that is already over.
        let invalid_arguments = tool.invalid_required_arguments(&input);
        // The verdict and its cause come from one judgement, taken here: the
        // loop decides whether to ask and what to tell the human from the same
        // answer, and a second evaluation after the run parks could see a mode
        // or rule that changed in between and describe a call the gate no
        // longer asks about.
        let approval = if invalid_arguments.is_empty() {
            tool.approval_reason(&input)
        } else {
            None
        };
        Self {
            id,
            tool,
            input,
            concurrency_safe,
            approval,
            invalid_arguments,
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
    pub(crate) async fn resolve_turn(
        &self,
        session: SessionId,
        response: &CompletionResponse,
        token: &CancellationToken,
    ) -> Result<NextStep, RunError> {
        let (mut plans, call_names) = self.plan_turn_calls(session, response).await?;
        // The calls in the order the model emitted them, taken from the
        // response before the hook gate removes the refused ones and before a
        // host miss drops an unknown name: a check is withheld only when the
        // edit it follows was among them.
        let emitted_calls = emitted_calls(response);
        let unknown_edits = unknown_edits(&emitted_calls, &plans);
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
        // keep it. Inject's input rewrite lands with the input-assembly cut.
        // The gate reads every call the model emitted, a call awaiting approval
        // included: a rule that refuses a call outranks a decision on it, so
        // nobody is asked to approve what the rule already refused.
        let blocked = self.run_pre_tool_use_gate(session, &mut plans).await;
        // The survivors split: a call its own tool gates on the input becomes
        // an approval request the caller resolves via resume(); the rest run.
        let mut approvals: Vec<ApprovalRequest> = Vec::new();
        let mut exec: Vec<ToolCallPlan> = Vec::new();
        // A call that cannot run is answered with the schema error the tool
        // would have raised, without executing it or asking anyone about it.
        let mut unrunnable: Vec<(String, Value)> = Vec::new();
        for plan in plans {
            if !plan.invalid_arguments.is_empty() {
                let message = format!(
                    "{}: {} required",
                    plan.tool.name(),
                    plan.invalid_arguments.join(", ")
                );
                unrunnable.push((plan.id, tool_error_json(&ToolError::InvalidInput(message))));
            } else if let Some(reason) = plan.approval {
                let ToolCallPlan {
                    id, tool, input, ..
                } = plan;
                approvals.push(ApprovalRequest::new(
                    id,
                    tool.name().to_string(),
                    input,
                    Some(reason),
                ));
            } else {
                exec.push(plan);
            }
        }
        // Execute in partition-by-safety batches (concurrency-safe runs
        // concurrent, non-safe serial); each result appends as it completes,
        // so the live delta renders per-tool progress rather than a batch
        // dump. Blocked and unrunnable calls had no execution, so they append
        // after; so do the checks behind an edit that will not have run, which
        // would read source the edit never wrote.
        let mut did_not_run: HashSet<String> = blocked
            .iter()
            .map(|(id, _)| id.clone())
            .chain(unrunnable.iter().map(|(id, _)| id.clone()))
            .chain(unknown_edits)
            .collect();
        // A call awaiting a verdict has written nothing yet, and its outcome
        // is not settled: a check behind it waits. The two batches that read
        // this disagree on purpose. The exec batch is dispatched now and
        // cannot wait, so its waiting checks are answered as skipped and the
        // model resends them once the edit lands. The approval batch can
        // wait: a check parked behind an undecided edit stays parked, and the
        // resume that approves the edit releases it in the same step.
        let awaiting: HashSet<String> = approvals
            .iter()
            .map(|request| request.call_id.clone())
            .collect();
        let outcomes = withhold::outcomes(&emitted_calls, &did_not_run, &awaiting);
        let withheld_now = withhold::check_actions(&emitted_calls, &outcomes);
        let mut without_effect = withheld_now.skip;
        without_effect.extend(withheld_now.wait);
        let withheld = withhold::take_withheld(&mut exec, &without_effect, |plan| plan.id.as_str());
        let mut results = self.execute_partitioned(session, &exec, token).await?;
        // An edit that failed while running settles its outcome only here,
        // after the calls waiting for approval were separated. The checks
        // among them are answered now: an approved check would otherwise run
        // against the source the failed edit left behind.
        did_not_run.extend(withhold::failed_edits(&results, &call_names));
        let outcomes = withhold::outcomes(&emitted_calls, &did_not_run, &awaiting);
        let settled = withhold::check_actions(&emitted_calls, &outcomes);
        let withheld_approvals =
            withhold::take_withheld(&mut approvals, &settled.skip, |request| {
                request.call_id.as_str()
            });
        // A withheld call never ran, but its call id still gets an answer: an
        // unanswered call reads as a lost result and makes the model repeat
        // work the turn already decided against.
        let answered: Vec<(String, Value)> =
            withheld.into_iter().chain(withheld_approvals).collect();
        self.append_not_run(session, &answered, &call_names).await?;
        self.append_not_run(session, &blocked, &call_names).await?;
        self.append_not_run(session, &unrunnable, &call_names)
            .await?;
        results.extend(answered);
        results.extend(blocked);
        results.extend(unrunnable);
        self.fold_tool_outcomes(&results, &call_names);
        if let Some(skill) = self.active_skill()
            && let Some(req) = self.skill_registry.as_ref().and_then(|registry| {
                let source = skill_source(&**registry, &skill)?;
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

    /// Classify the calls the model emitted, in emission order. A call naming
    /// no registered tool is answered here and left out of the plans: the host
    /// cannot dispatch it, and the model needs the result to stop repeating it.
    async fn plan_turn_calls(
        &self,
        session: SessionId,
        response: &CompletionResponse,
    ) -> Result<(Vec<ToolCallPlan>, HashMap<String, String>), RunError> {
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
        Ok((plans, call_names))
    }

    /// Record the answer for each call the turn answered without running it.
    /// The name is the one the model wrote, which is what the large-output
    /// path keys on; the recorded result reaches its call by call id.
    async fn append_not_run(
        &self,
        session: SessionId,
        answers: &[(String, Value)],
        call_names: &HashMap<String, String>,
    ) -> Result<(), RunError> {
        for (id, output) in answers {
            let tool = call_names.get(id).map(String::as_str).unwrap_or("");
            self.append_tool_result(session, id.clone(), tool, output.clone(), 0)
                .await?;
        }
        Ok(())
    }

    /// Fold one batch of tool outcomes into the session tallies: the tool
    /// counts behind /context and the observability log. Called by the turn
    /// that dispatched the calls and by the resume that released them, so the
    /// counts cover every call that ran, whichever path released it.
    pub(crate) fn fold_tool_outcomes(
        &self,
        results: &[(String, Value)],
        call_names: &HashMap<String, String>,
    ) {
        let counts = outcome_counts::count_tool_outcomes(results);
        if let Ok(mut g) = self.usage.lock() {
            g.record_tool_batch(&counts);
        }
        obs_wire::record_tool_outcomes(&self.observability, results, call_names);
    }
}

/// Every call the model emitted, in order, with the tool name it gave. Taken
/// from the response rather than from the planned calls: a name the host does
/// not resolve still marks a call the model expects to have run.
fn emitted_calls(response: &CompletionResponse) -> Vec<(String, String)> {
    response
        .output
        .iter()
        .filter_map(|item| match item {
            OutputItem::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
            _ => None,
        })
        .collect()
}

/// The emitted calls whose tool the host did not resolve and whose name is a
/// file edit. None of them ran, so the checks sent after them must not either.
fn unknown_edits(emitted: &[(String, String)], plans: &[ToolCallPlan]) -> Vec<String> {
    let planned: HashSet<&str> = plans.iter().map(|plan| plan.id.as_str()).collect();
    emitted
        .iter()
        .filter(|(id, name)| !planned.contains(id.as_str()) && writes_files(name))
        .map(|(id, _)| id.clone())
        .collect()
}

/// Scan executed shell results for authorizable_services (mach services
/// the sandbox blocked during a failed command). Only the shell family is
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
        let is_bash = call_names.get(id).is_some_and(|name| is_shell_tool(name));
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
                // Raised from a tool result, not from a gate verdict: the
                // sandbox reported the blocked services after the call ran,
                // so there is no cause the gate can name.
                None,
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests;
