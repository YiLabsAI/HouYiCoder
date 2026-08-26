//! Turn resolution: dispatch one turn's tool calls and compute the next step.
//!
//! Extracted from the runner module so the runner file stays under the size
//! gate and turn resolution is its own concern: collect executable +
//! approval-requiring calls, observe redundant calls, arbitrate PreToolUse
//! hooks, execute the partitioned batches, record outcomes, and decide
//! RunAgain / FinalOutput / Interruption. The runner's drive loop calls this
//! once per model response.

use std::sync::Arc;

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
        let mut call_names: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
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
