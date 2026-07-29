//! Hook fire-point wiring: PreToolUse arbitration before tool execution +
//! PostToolUse / PostToolUseFailure non-blocking fire after. Split from
//! mod.rs so the runner surface stays under the file-size gate. These are
//! pub(super) helpers called from apply_decisions' tool-exec path.
//!
//! Verdict handling (full, per the hook design):
//! - Allow / Observe / Trigger / Inject keep the tool in the exec queue.
//!   Inject rewrites the tool input (the design's updatedInput); the
//!   rewrite lands with the input-projection cut -- for now the input is
//!   kept unchanged and the inject content is recorded as an observation.
//! - Deny / Feedback / Ask remove the tool + return a synthetic blocked
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

use std::sync::Arc;

use houyicoder_api::tool::Tool;
use houyicoder_context::SessionId;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{
    HookContext, HookEvent, HookOutcome, HookPayload, HookRegistry, HookVerdict, ToolResult,
    arbitrate,
};
use crate::agent::Runner;

/// A ProgressSink that forwards tool progress to the runner's live-event
/// stream as ToolProgress (the host renders (Ns) on the chip). One per tool
/// call — carries the call_id so the host routes the tick to the right chip.
/// None when no live sink is wired (tests, non-interactive runs); the
/// progress calls are then no-ops. Held by the ToolCtx for the call's
/// lifetime; the tool drives progress through it (e.g. BashTool ticks
/// elapsed every ~1s).
struct LiveProgressSink {
    call_id: String,
    live: Option<houyicoder_api::live::LiveSink>,
}

impl LiveProgressSink {
    fn new(call_id: String, live: Option<houyicoder_api::live::LiveSink>) -> Self {
        Self { call_id, live }
    }
}

impl houyicoder_api::progress::ProgressSink for LiveProgressSink {
    fn progress(&self, current: u64, total: Option<u64>) {
        // current is the tool's elapsed seconds; total is the running stdout
        // line count (Some when the backend streams, None otherwise). Forward
        // both so the host's chip renders "(Ns · M lines)".
        if let Some(live) = &self.live {
            live(&houyicoder_api::live::LiveEvent::ToolProgress {
                call_id: self.call_id.clone(),
                elapsed_secs: current,
                lines: total,
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
            self.emit_system_line(format!(
                "untrusted project hooks skipped: {} (use /trust to enable)",
                skipped.join(", ")
            ));
        }
        outcomes
    }

    /// Fire PreToolUse for every tool about to execute, arbitrate the
    /// verdicts, and partition the exec queue: Allow / Observe / Trigger /
    /// Inject keep the call (Inject TODO rewrites input); Deny / Feedback /
    /// Ask remove it + return a synthetic blocked result so the model sees
    /// the reason. Mutates the exec queue in place; returns the blocked
    /// results.
    pub(crate) async fn arbitrate_pre_tool_use(
        &self,
        session: SessionId,
        exec: &mut Vec<(String, Arc<dyn Tool>, Value, bool)>,
    ) -> Vec<(String, Value)> {
        let Some(reg) = self.hooks.as_ref() else {
            return Vec::new();
        };
        let mut blocked: Vec<(String, Value)> = Vec::new();
        let mut kept: Vec<(String, Arc<dyn Tool>, Value, bool)> = Vec::with_capacity(exec.len());
        for (id, tool, input, safe) in exec.drain(..) {
            let tool_name = tool.name().to_string();
            let ctx = HookContext {
                event: HookEvent::PreToolUse,
                payload: HookPayload::PreToolUse {
                    tool_name: tool_name.clone(),
                    input: input.clone(),
                    backfilled_input: None,
                },
                session,
            };
            let outcomes = self.dispatch_hooks(reg, &ctx);
            self.append_hook_signals(session, HookEvent::PreToolUse, Some(&tool_name), &outcomes)
                .await;
            let verdict = arbitrate(outcomes.into_iter().map(|o| o.result));
            let allow = match verdict.primary {
                HookVerdict::Allow => true,
                HookVerdict::Inject(_) => {
                    // TODO: rewrite the tool input (updatedInput). For now
                    // keep the input; the inject content is already recorded
                    // as an observation by the arbitrate pass above.
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
                    if let Some(memory) = self.memory.as_ref() {
                        memory.record_gate_violation(&reason);
                    }
                    blocked.push((id.clone(), hook_blocked_json(&reason)));
                    false
                }
                HookVerdict::Feedback(reason) => {
                    blocked.push((id.clone(), hook_feedback_json(&reason)));
                    false
                }
                HookVerdict::Ask(question) => {
                    // TODO: thread through the interruption path as a hook-Ask
                    // (the same machinery as tool approval). For now block +
                    // surface the question so the model can answer it.
                    blocked.push((
                        id.clone(),
                        hook_blocked_json(&format!("hook asks: {question}")),
                    ));
                    false
                }
            };
            if allow {
                kept.push((id, tool, input, safe));
            }
        }
        *exec = kept;
        blocked
    }

    /// Execute the (PreToolUse-filtered) tool calls in partition-by-safety
    /// batches: a maximal run of concurrency-safe calls runs concurrently
    /// via FuturesUnordered (results land in completion order, not input
    /// order); a non-safe call runs alone, serially, so a mutating tool
    /// never overlaps another. PostToolUse / PostToolUseFailure fires after
    /// each call. Correct transcript pairing relies on call_id uniqueness
    /// (established at the provider boundary), not on result ordering.
    #[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
    pub(crate) async fn execute_partitioned(
        &self,
        session: SessionId,
        exec: &[(String, Arc<dyn Tool>, Value, bool)],
        token: &CancellationToken,
    ) -> Result<Vec<(String, Value)>, super::super::RunError> {
        use crate::agent::synthetic::{SyntheticToolOutcome, tool_error_json};
        use futures::stream::{FuturesUnordered, StreamExt};
        use houyicoder_api::tool::ToolCtx;
        use std::collections::HashSet;
        let mut results: Vec<(String, Value)> = Vec::with_capacity(exec.len());
        let mut i = 0;
        while i < exec.len() {
            if exec[i].3 {
                // Parallel batch: the maximal run of safe calls from i. Each
                // result is appended + fired as it completes, so the live
                // delta shows per-tool progress (a streaming render), not a
                // single batch dump when the slowest call returns.
                let mut j = i;
                while j < exec.len() && exec[j].3 {
                    j += 1;
                }
                let batch: Vec<(String, String, Arc<dyn Tool>, Value)> = exec[i..j]
                    .iter()
                    .map(|(id, t, input, _)| {
                        (id.clone(), t.name().to_string(), t.clone(), input.clone())
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
                let live = self.live.clone();
                let mut group: FuturesUnordered<_> = batch
                    .into_iter()
                    .map(move |(id, name, t, input)| {
                        let live = live.clone();
                        async move {
                            let input_for_hook = input.clone();
                            // Measure the wall-clock length of this one tool call so
                            // the durable ToolResult carries per-call latency for
                            // /trajectory's gantt + the ExPeL slow-tool mining. The
                            // start is captured inside the per-call future so each
                            // result carries its OWN duration, not the batch's.
                            let start = std::time::Instant::now();
                            // Propagate the run's CancellationToken into ToolCtx so
                            // a tool that honors ctx.cancel (Grep/Glob) observes
                            // the abort mid-walk and returns promptly. Attach the
                            // live progress sink so a long-running tool (bash) can
                            // tick elapsed back to the host's chip.
                            let r = t
                                .execute(
                                    ToolCtx::new(id.as_str())
                                        .with_cancel(token.clone())
                                        .with_session(session)
                                        .with_progress(std::sync::Arc::new(LiveProgressSink::new(
                                            id.clone(),
                                            live,
                                        ))),
                                    input,
                                )
                                .await;
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
                                let o = SyntheticToolOutcome::Interrupted.to_json();
                                let is_error = crate::observability::tool_failure_reason(&o).is_some();
                                self.fire_post_tool_use(session, id, name, input, &o, is_error)
                                    .await;
                                self.record_redundancy(name, input, is_error);
                                self.append_tool_result(session, id.clone(), name, o.clone(), 0)
                                    .await?;
                                results.push((id.clone(), o));
                                completed.insert(id.clone());
                            }
                            // Drop the in-flight futures so a blocking tool
                            // future does not keep the run alive past Esc.
                            group.clear();
                            break;
                        }
                        out = group.next() => {
                            let Some((id, name, input, o, duration_ms)) = out else { break; };
                            let is_error = crate::observability::tool_failure_reason(&o).is_some();
                            self.fire_post_tool_use(session, &id, &name, &input, &o, is_error)
                                .await;
                            self.record_redundancy(&name, &input, is_error);
                            self.append_tool_result(session, id.clone(), &name, o.clone(), duration_ms)
                                .await?;
                            completed.insert(id.clone());
                            results.push((id, o));
                        }
                    }
                }
                i = j;
            } else {
                // Serial: a non-safe call runs alone.
                let (id, t, input, _) = &exec[i];
                let id = id.clone();
                let name = t.name().to_string();
                let input = input.clone();
                let exec_fut = t.execute(
                    ToolCtx::new(id.as_str())
                        .with_cancel(token.clone())
                        .with_session(session)
                        .with_progress(std::sync::Arc::new(LiveProgressSink::new(
                            id.clone(),
                            self.live.clone(),
                        ))),
                    input.clone(),
                );
                let start = std::time::Instant::now();
                let (r, cancelled) = tokio::select! {
                    _ = token.cancelled() => (Ok(SyntheticToolOutcome::Interrupted.to_json()), true),
                    r = exec_fut => (r, false),
                };
                // No duration on the cancel path: the call was interrupted, so
                // no real execution completed to time.
                let duration_ms = if cancelled {
                    0
                } else {
                    start.elapsed().as_millis() as u64
                };
                let o = match r {
                    Ok(v) => v,
                    Err(e) => tool_error_json(&e),
                };
                let is_error = crate::observability::tool_failure_reason(&o).is_some();
                self.fire_post_tool_use(session, &id, &name, &input, &o, is_error)
                    .await;
                self.record_redundancy(&name, &input, is_error);
                self.append_tool_result(session, id.clone(), &name, o.clone(), duration_ms)
                    .await?;
                results.push((id, o));
                i += 1;
            }
        }
        Ok(results)
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
        // PostToolUse is non-blocking: arbitrate collects triggers for the
        // (future) async trigger-dispatch seam; the primary verdict is not
        // acted on here, so it is not bound. Per-hook signals are already
        // recorded above.
        let _verdict = arbitrate(outcomes.into_iter().map(|o| o.result));
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
#[path = "hook_signal_tests.rs"]
mod hook_signal_tests;
