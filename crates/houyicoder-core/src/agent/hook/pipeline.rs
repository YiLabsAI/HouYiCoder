//! Hook verdict combination and lifecycle events around tool execution.
//!
//! Verdict handling (full, per the hook design):
//! - Allow / Observe / Trigger / Inject keep the tool in the exec queue.
//!   Inject rewrites the tool input (the design's updatedInput); the
//!   rewrite lands with the input-projection cut -- for now the input is
//!   kept unchanged and the inject content is recorded as an observation.
//! - Deny / Feedback / Ask remove the tool + return a blocked
//!   result so the model sees the reason losslessly. Deny is terminal (no
//!   retry); Feedback surfaces a self-correction signal the model can act
//!   on with adjusted input; Ask escalates to the user -- the deeper
//!   integration threads the question through the interruption path (the
//!   same machinery as tool approval), a follow-up. For now Ask blocks +
//!   surfaces the question so the model can answer it.
//!

//! Observations + triggers are non-blocking; they are recorded even when a
//! blocking verdict is present (the core advantage over a single-verdict
//! return). Trigger async-fire machinery lands with the trigger-dispatch cut.

use std::collections::HashSet;
use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use houyicoder_api::agent_event::{EventHandler, ToolExecutionEvent};
use houyicoder_api::tool::progress::ToolProgressReporter;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_context::SessionId;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{
    HookContext, HookEvent, HookOutcome, HookPayload, HookRegistry, HookVerdict, ToolResult,
    combine_verdicts,
};
use crate::agent::fallback::{FallbackToolOutcome, tool_error_json};
use crate::agent::resolve::ToolCallPlan;
use crate::agent::{RunError, Runner};

/// Forwards progress from one tool call to the tool-execution event handler.
/// The call identity lets the host route each update to the correct tool.
struct ToolExecutionReporter {
    call_id: String,
    handler: Option<Arc<dyn EventHandler<ToolExecutionEvent>>>,
}

impl ToolExecutionReporter {
    fn new(call_id: String, handler: Option<Arc<dyn EventHandler<ToolExecutionEvent>>>) -> Self {
        Self { call_id, handler }
    }
}

impl ToolProgressReporter for ToolExecutionReporter {
    fn progress(&self, current: u64, total: Option<u64>) {
        if let Some(handler) = &self.handler {
            handler.handle(ToolExecutionEvent::Progress {
                call_id: self.call_id.clone(),
                elapsed_secs: current,
                output_lines: total,
            });
        }
    }
}

impl Runner {
    /// Dispatch a hook event and surface the one-time untrusted-skip notice.
    /// The registry queues the notice (it has no channel to the user); the
    /// runner drains it here so every dispatch site gets the system line
    /// without re-writing the notice logic.
    pub(crate) fn dispatch_hooks(&self, reg: &HookRegistry, ctx: &HookContext) -> Vec<HookOutcome> {
        let outcomes = reg.dispatch(ctx);
        if let Some(skipped) = reg.take_skipped_untrusted() {
            // The trust gate (TrustState::Untrusted) is scaffolded but not
            // enforced today: no project-level hook source is wired yet, and
            // nothing sets the registry to Untrusted, so this branch is
            // unreachable. The message names no escape hatch because there
            // is none to name; when a real project hook source lands, the
            // surfacing here is the place to wire its trust prompt.
            self.emit_system_line(format!(
                "untrusted project hooks skipped: {}",
                skipped.join(", ")
            ));
        }
        outcomes
    }

    /// Fire PreToolUse for every tool about to execute, merge the verdicts,
    /// and partition the queue: Allow / Observe / Trigger / Inject keep
    /// the call (Inject TODO rewrites input); Deny / Feedback / Ask remove
    /// it + return a blocked result so the model sees the reason. Mutates
    /// the queue in place; returns the blocked results.
    pub(crate) async fn run_pre_tool_use_gate(
        &self,
        session: SessionId,
        plans: &mut Vec<ToolCallPlan>,
    ) -> Vec<(String, Value)> {
        let Some(reg) = self.hooks.as_ref() else {
            return Vec::new();
        };
        let mut blocked: Vec<(String, Value)> = Vec::new();
        let mut kept: Vec<ToolCallPlan> = Vec::with_capacity(plans.len());
        for plan in plans.drain(..) {
            let tool_name = plan.tool.name().to_string();
            let ctx = HookContext {
                event: HookEvent::PreToolUse,
                payload: HookPayload::PreToolUse {
                    tool_name: tool_name.clone(),
                    input: plan.input.clone(),
                    backfilled_input: None,
                },
                session,
            };
            let outcomes = self.dispatch_hooks(reg, &ctx);
            self.append_hook_signals(session, HookEvent::PreToolUse, Some(&tool_name), &outcomes)
                .await;
            let verdict = combine_verdicts(outcomes.into_iter().map(|o| o.result));
            let allow = match verdict.primary {
                HookVerdict::Allow => true,
                HookVerdict::Inject(_) => {
                    // TODO: rewrite the tool input (updatedInput). For now
                    // keep the input; the inject content is already recorded
                    // as an observation by the combine_verdicts pass above.
                    true
                }
                HookVerdict::Observe(_) | HookVerdict::Trigger(_) => true,
                HookVerdict::Deny(reason) => {
                    // Signal B: a PreToolUse gate denied a call. Record a
                    // violation against the deny reason (the rule the agent
                    // violated) so the consolidation dream can nominate the
                    // rule for promotion into the always-on carrier. The
                    // reason is best-effort sanitized to a memory key; a
                    // hook whose deny reason names the rule key lands a
                    // precise counter, a free-text reason lands a coarse
                    // one. Either way the dream sees the cumulative count.
                    if let Some(memory) = self.memory.provider() {
                        memory.record_gate_violation(&reason);
                    }
                    blocked.push((plan.id.clone(), hook_blocked_json(&reason)));
                    false
                }
                HookVerdict::Feedback(reason) => {
                    blocked.push((plan.id.clone(), hook_feedback_json(&reason)));
                    false
                }
                HookVerdict::Ask(question) => {
                    // TODO: thread through the interruption path as a hook-Ask
                    // (the same machinery as tool approval). For now block +
                    // surface the question so the model can answer it.
                    blocked.push((
                        plan.id.clone(),
                        hook_blocked_json(&format!("hook asks: {question}")),
                    ));
                    false
                }
            };
            if allow {
                kept.push(plan);
            }
        }
        *plans = kept;
        blocked
    }

    /// Execute the calls that survived PreToolUse arbitration, in
    /// partition-by-safety batches: a maximal run of concurrency-safe calls
    /// runs together through FuturesUnordered, so their results land in
    /// completion order rather than call order, and a non-safe call runs
    /// alone so a mutating tool never overlaps another. The transcript
    /// pairs each result with its tool_use by call_id (unique from the
    /// provider boundary), which is what makes that ordering safe.
    pub(crate) async fn execute_partitioned(
        &self,
        session: SessionId,
        plans: &[ToolCallPlan],
        token: &CancellationToken,
    ) -> Result<Vec<(String, Value)>, RunError> {
        let mut results: Vec<(String, Value)> = Vec::with_capacity(plans.len());
        let mut i = 0;
        while i < plans.len() {
            if plans[i].concurrency_safe {
                // Parallel batch: the maximal run of safe calls from i. Each
                // result is appended + fired as it completes, so the live
                // delta shows per-tool progress (a streaming render), not a
                // single batch dump when the slowest call returns.
                let mut j = i;
                while j < plans.len() && plans[j].concurrency_safe {
                    j += 1;
                }
                let batch: Vec<(String, String, Arc<dyn Tool>, Value)> = plans[i..j]
                    .iter()
                    .map(|plan| {
                        (
                            plan.id.clone(),
                            plan.tool.name().to_string(),
                            plan.tool.clone(),
                            plan.input.clone(),
                        )
                    })
                    .collect();
                // Snapshot (id, name, input) for the cancel path: the group
                // owns the tool Arc + input, so on an Esc mid-batch we need
                // the ids/inputs here to emit interrupted results for the
                // calls that have not completed yet.
                let cancel_keys: Vec<(String, String, Value)> = batch
                    .iter()
                    .map(|(id, name, _, input)| (id.clone(), name.clone(), input.clone()))
                    .collect();
                let mut group: FuturesUnordered<_> = batch
                    .into_iter()
                    .map(move |(id, name, t, input)| {
                        async move {
                            let input_for_hook = input.clone();
                            // Measure the wall-clock length of this one tool call so
                            // the durable ToolResult carries per-call latency for
                            // /trajectory's gantt + the ExPeL slow-tool mining. The
                            // start is captured inside the per-call future so each
                            // result carries its OWN duration, not the batch's.
                            let start = std::time::Instant::now();
                            // The ctx carries the run token, so a tool
                            // honoring cancellation (Grep/Glob) returns
                            // promptly on a stop; the batch select below drops
                            // whatever is still running in the group.
                            let ctx = self.tool_ctx(id.as_str(), session, token);
                            let r = t.execute(ctx, input).await;
                            let duration_ms = start.elapsed().as_millis() as u64;
                            let o = match r {
                                Ok(v) => v,
                                Err(e) => tool_error_json(&e),
                            };
                            (id, name, input_for_hook, o, duration_ms)
                        }
                    })
                    .collect();
                let mut completed: HashSet<String> = HashSet::new();
                loop {
                    tokio::select! {
                        biased;
                        // Esc mid-batch: calls that already completed have
                        // their results appended (preserved); the rest get an
                        // interrupted result so the run resolves instead of
                        // waiting for a blocking tool future to return.
                        _ = token.cancelled(), if !group.is_empty() => {
                            for (id, name, input) in &cancel_keys {
                                if completed.contains(id) {
                                    continue;
                                }
                                let output =
                                    self.record_interrupted(session, id, name, input).await?;
                                results.push((id.clone(), output));
                                completed.insert(id.clone());
                            }
                            // Drop the running futures so a blocking tool
                            // future does not keep the run alive past Esc.
                            group.clear();
                            break;
                        }
                        out = group.next() => {
                            let Some((id, name, input, o, duration_ms)) = out else { break; };
                            self.record_result(session, &id, &name, &input, &o, duration_ms)
                                .await?;
                            completed.insert(id.clone());
                            results.push((id, o));
                        }
                    }
                }
                i = j;
            } else {
                // Serial: a non-safe call runs alone.
                let plan = &plans[i];
                let o = self
                    .execute_call(session, &plan.id, &plan.tool, plan.input.clone(), token)
                    .await?;
                results.push((plan.id.clone(), o));
                i += 1;
            }
        }
        Ok(results)
    }

    /// Run one tool call alone through the tool's own approval gate,
    /// recording PostToolUse, the redundancy observation, and the durable
    /// result.
    pub(crate) async fn execute_call(
        &self,
        session: SessionId,
        id: &str,
        tool: &Arc<dyn Tool>,
        input: Value,
        token: &CancellationToken,
    ) -> Result<Value, RunError> {
        self.dispatch_call(session, id, tool, input, token, false)
            .await
    }

    /// The resume path's entry: the call was approved, so it runs the tool's
    /// authorized entry point — an answered ask is not raised again — under
    /// the same capabilities and cancellation race as a call the loop
    /// dispatched.
    pub(crate) async fn execute_approved_call(
        &self,
        session: SessionId,
        id: &str,
        tool: &Arc<dyn Tool>,
        input: Value,
        token: &CancellationToken,
    ) -> Result<Value, RunError> {
        self.dispatch_call(session, id, tool, input, token, true)
            .await
    }

    /// Dispatch one call: attach the per-call capability set, race it against
    /// the run token so an abort drops the tool future (the sandbox guards
    /// inside it kill the process), then record PostToolUse or
    /// PostToolUseFailure, the redundancy observation, and the durable
    /// ToolResult.
    async fn dispatch_call(
        &self,
        session: SessionId,
        id: &str,
        tool: &Arc<dyn Tool>,
        input: Value,
        token: &CancellationToken,
        approved: bool,
    ) -> Result<Value, RunError> {
        let name = tool.name().to_string();
        let ctx = self.tool_ctx(id, session, token);
        let exec_fut = if approved {
            tool.execute_authorized(ctx, input.clone())
        } else {
            tool.execute(ctx, input.clone())
        };
        let start = std::time::Instant::now();
        let (r, cancelled) = tokio::select! {
            _ = token.cancelled() => (Ok(FallbackToolOutcome::Interrupted.to_json()), true),
            r = exec_fut => (r, false),
        };
        // No duration on the cancel path: the call was interrupted, so no
        // real execution completed to time.
        let duration_ms = if cancelled {
            0
        } else {
            start.elapsed().as_millis() as u64
        };
        let output = match r {
            Ok(v) => v,
            Err(e) => tool_error_json(&e),
        };
        self.record_result(session, id, &name, &input, &output, duration_ms)
            .await?;
        Ok(output)
    }

    /// Record one finished call: fire PostToolUse or PostToolUseFailure, note
    /// the redundancy observation, and append the durable result. Every path
    /// that finishes a call lands here, so a call dispatched by the loop, a
    /// call inside a parallel batch, and a call released by an approval
    /// decision cannot record different things.
    async fn record_result(
        &self,
        session: SessionId,
        id: &str,
        name: &str,
        input: &Value,
        output: &Value,
        duration_ms: u64,
    ) -> Result<(), RunError> {
        let is_error = crate::observability::tool_failure_reason(output).is_some();
        self.fire_post_tool_use(session, id, name, input, output, is_error)
            .await;
        self.record_redundancy(name, input, is_error);
        self.append_tool_result(session, id.to_string(), name, output.clone(), duration_ms)
            .await
    }

    /// Record a call the run stopped before it could produce a result, through
    /// the same path a finished call takes: hooks and the redundancy observer
    /// see the interrupted outcome, and the session keeps a result for its
    /// tool_use. Returns the payload it recorded so the caller folds the same
    /// outcome into the session tallies.
    pub(crate) async fn record_interrupted(
        &self,
        session: SessionId,
        id: &str,
        name: &str,
        input: &Value,
    ) -> Result<Value, RunError> {
        let output = FallbackToolOutcome::Interrupted.to_json();
        self.record_result(session, id, name, input, &output, 0)
            .await?;
        Ok(output)
    }

    /// The capability set one tool call executes with: the run token a tool
    /// honoring cancellation returns on, the session, the denied agent set,
    /// the progress forwarder, the agent identity, and the spawn / hook-fire
    /// seams when wired. One builder, so a dispatched call and an approved
    /// call cannot drift apart.
    fn tool_ctx(&self, call_id: &str, session: SessionId, token: &CancellationToken) -> ToolCtx {
        let mut ctx = ToolCtx::new(call_id)
            .with_cancel(token.clone())
            .with_session(session)
            .with_denied_agents(self.denied_agents.clone())
            .with_progress(Arc::new(ToolExecutionReporter::new(
                call_id.to_string(),
                self.events.tool_execution_handler(),
            )))
            .with_agent_identity(self.agent_identity().clone());
        if let Some(h) = self.spawn_handle() {
            ctx = ctx.with_spawn_handle(h.clone());
        }
        if let Some(hf) = super::fire::build_hook_fire(self) {
            ctx = ctx.with_hook_fire(hf);
        }
        ctx
    }

    /// Fire PostToolUse (success) or PostToolUseFailure (error) after a tool
    /// ran. Non-blocking: observations are recorded, triggers fire
    /// downstream. A hook error here is logged, never panics the run.
    pub(super) async fn fire_post_tool_use(
        &self,
        session: SessionId,
        _id: &str,
        tool_name: &str,
        input: &Value,
        output: &Value,
        is_error: bool,
    ) {
        let Some(reg) = self.hooks.as_ref() else {
            return;
        };
        let payload = if is_error {
            HookPayload::PostToolUseFailure {
                tool_name: tool_name.to_string(),
                error: crate::observability::tool_failure_reason(output)
                    .map(|r| r.into_owned())
                    .unwrap_or_else(|| "tool error".to_string()),
            }
        } else {
            HookPayload::PostToolUse {
                tool_name: tool_name.to_string(),
                input: input.clone(),
                result: ToolResult {
                    output: output.to_string(),
                },
            }
        };
        let event = if is_error {
            HookEvent::PostToolUseFailure
        } else {
            HookEvent::PostToolUse
        };
        let ctx = HookContext {
            event,
            payload,
            session,
        };
        let outcomes = self.dispatch_hooks(reg, &ctx);
        self.append_hook_signals(session, event, Some(tool_name), &outcomes)
            .await;
        // PostToolUse is non-blocking: combine_verdicts collects triggers for the
        // (future) async trigger-dispatch seam; the primary verdict is not
        // acted on here, so it is not bound. Per-hook signals are already
        // recorded above.
        let _verdict = combine_verdicts(outcomes.into_iter().map(|o| o.result));
    }

    /// Record one executed tool call's outcome into the redundant-call
    /// tracker (harness self-evolution observer). Independent of the hook
    /// registry — runs unconditionally, brief pure compute under the Mutex.
    fn record_redundancy(&self, tool_name: &str, input: &Value, is_error: bool) {
        if let Ok(mut t) = self.redundancy.lock() {
            t.record(tool_name, input, is_error);
        }
    }

    /// Whether a hook registry is wired (the runner fires hooks at runtime).
    #[cfg(test)]
    pub(super) fn hooks_wired(&self) -> bool {
        self.hooks.is_some()
    }
}

/// Model-visible JSON for a tool call blocked by a Deny verdict. The model
/// sees the reason losslessly + can retry with adjusted input (Deny is
/// terminal for this call, not for the run).
fn hook_blocked_json(reason: &str) -> Value {
    serde_json::json!({ "error": format!("blocked by hook: {reason}") })
}

/// Model-visible JSON for a tool call surfaced a Feedback verdict. The model
/// sees the self-correction signal + can retry with adjusted input.
fn hook_feedback_json(reason: &str) -> Value {
    serde_json::json!({ "error": format!("hook feedback: {reason}") })
}

#[cfg(test)]
#[path = "fire_tests.rs"]
mod fire_tests;

#[cfg(test)]
#[path = "fire_seam_tests.rs"]
mod fire_seam_tests;

#[cfg(test)]
#[path = "hook_signal_tests.rs"]
mod hook_signal_tests;
