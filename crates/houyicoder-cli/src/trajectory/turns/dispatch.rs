//! Routing durable events into the turn they belong to.
//!
//! The builder owns one turn's state; this module owns which event goes to it,
//! and which events open or close a turn.

use std::collections::HashSet;

use houyicoder_context::{HookVerdictKind, SessionEvent, SessionLogEntry};
use houyicoder_tui::state::TrajectoryTurnKey;
use houyicoder_tui::view::trajectory_pane::{
    CompactedBoundary, EventUsage, ModelSwitchBoundary, TrajectoryRecord, TrajectoryRecordKind,
    TrajectoryTurn, TurnBoundary,
};

use super::super::view::{CallIndex, fmt_bytes, preview, result_failed};
use super::TurnBuilder;

/// Open or close turns. Returns true when the event was a turn boundary, so
/// the caller does not also fold it in as turn content.
///
/// A user input opens a turn. The model-call boundary that follows stays inside
/// it, so a prompt that needs several round trips is still one turn.
pub(in crate::trajectory) fn apply_turn_boundary(
    builder: &mut TurnBuilder,
    ev: &SessionLogEntry,
    turns: &mut Vec<TrajectoryTurn>,
    n: &mut usize,
    pending_boundary: &mut Vec<TurnBoundary>,
    last_usage: &mut Option<(String, u64)>,
    records: &mut Vec<(TrajectoryTurnKey, Vec<TrajectoryRecord>)>,
) -> bool {
    match &ev.event {
        SessionEvent::ContextCleared { prior_turn } => {
            pending_boundary.push(TurnBoundary::ContextCleared {
                prior_turn: *prior_turn,
                at_secs: ev.ts / 1000,
            });
        }
        SessionEvent::CompactionBoundary {
            checkpoint,
            pre_tokens,
            post_tokens,
        } => {
            pending_boundary.push(TurnBoundary::Compacted(Box::new(CompactedBoundary {
                checkpoint_id: checkpoint.to_string(),
                pre_tokens: *pre_tokens,
                post_tokens: *post_tokens,
                at_secs: ev.ts / 1000,
            })));
        }
        SessionEvent::TurnUsage { model, .. } => {
            if !model.is_empty() {
                if let Some((prev, _)) = last_usage.as_ref()
                    && prev != model
                {
                    let boundary = TurnBoundary::ModelSwitch(Box::new(ModelSwitchBoundary {
                        from: prev.clone(),
                        to: model.clone(),
                        at_secs: ev.ts / 1000,
                    }));
                    if !builder.is_open() {
                        pending_boundary.push(boundary);
                    } else if !builder.has_usage() {
                        // The turn's first usage: the switch happened between
                        // turns, so it belongs above this one. A later usage is
                        // a switch inside the turn, which its own model records
                        // already show.
                        builder.boundary_before.push(boundary);
                    }
                }
                *last_usage = Some((model.clone(), ev.ts));
            }
            return false;
        }
        SessionEvent::UserInput { text } => {
            if builder.is_open()
                && let Some(entry) = builder.flush(turns, *n)
            {
                records.push(entry);
            }
            *n += 1;
            builder.reset(ev, text.clone(), std::mem::take(pending_boundary));
            let record = builder.record(TrajectoryRecordKind::Context, None, preview(text), ev.ts);
            let index = builder.push(record);
            builder.set_input(index, text.clone());
        }
        SessionEvent::MidTurnInput { text, .. } => {
            builder.touch(ev.ts);
            let summary = format!("User update: {}", preview(text));
            let record = builder.record(
                TrajectoryRecordKind::Context,
                Some("User update".into()),
                summary,
                ev.ts,
            );
            let index = builder.push(record);
            builder.set_input(index, text.clone());
        }
        SessionEvent::TurnStarted { .. } => {
            // A log that opens mid-run has model calls before any user input;
            // open a turn so those records have a home.
            if !builder.is_open() {
                *n += 1;
                builder.reset(ev, String::new(), std::mem::take(pending_boundary));
            }
            builder.open_model_call(None, ev.ts);
        }
        _ => return false,
    }
    true
}

/// Fold an event that belongs to the turn already open.
pub(in crate::trajectory) fn apply_turn_content(
    builder: &mut TurnBuilder,
    ev: &SessionLogEntry,
    calls: &CallIndex,
    spawned: &HashSet<String>,
) {
    match &ev.event {
        SessionEvent::ModelStepTiming {
            total_ms,
            ttft_ms,
            decode_ms,
            ..
        } => builder.attach_timing(*total_ms, *ttft_ms, *decode_ms, ev.ts),
        SessionEvent::TurnUsage { .. } => builder.apply_usage(&ev.event, ev.ts),
        SessionEvent::AssistantMessage { text, thinking } => {
            builder.attach_assistant(text, thinking, ev.ts);
        }
        SessionEvent::Reasoning { text } => builder.attach_reasoning(text, ev.ts),
        // A delegation's own tool call is represented by its Agent record;
        // showing the tool as well would count the same work twice.
        SessionEvent::ToolCall {
            call_id,
            tool,
            input,
        } => {
            if !spawned.contains(call_id) {
                builder.begin_tool(call_id, tool, input, ev.ts);
            }
        }
        SessionEvent::ToolResult {
            call_id,
            output,
            duration_ms,
        } => {
            if spawned.contains(call_id) {
                return;
            }
            let failed = result_failed(output, call_id, calls);
            builder.finish_tool(call_id, output, *duration_ms, failed, ev.ts);
        }
        SessionEvent::SubagentSpawn {
            child_session_id,
            subagent_type,
            prompt_summary,
            ..
        } => builder.begin_agent(child_session_id, subagent_type, prompt_summary, ev.ts),
        SessionEvent::SubagentReturn {
            child_session_id,
            status,
            summary,
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_tokens,
            ..
        } => {
            let usage = (*input_tokens > 0 || *output_tokens > 0).then_some(EventUsage {
                input: Some(*input_tokens),
                output: Some(*output_tokens),
                cache_read: Some(*cache_read_input_tokens),
                cache_write: Some(*cache_write_input_tokens),
                reasoning: Some(*reasoning_tokens),
            });
            builder.finish_agent(child_session_id, status, summary, usage, ev.ts);
        }
        SessionEvent::MemoryRecall { keys, bytes, .. } => {
            builder.touch(ev.ts);
            // The row names the recall and its count; the size and the keys
            // themselves belong to the drill-down, where there is room for
            // them.
            let summary = if keys.is_empty() {
                "Recall".to_string()
            } else {
                format!("Recall {} items", keys.len())
            };
            let record = builder.record(TrajectoryRecordKind::Memory, None, summary, ev.ts);
            let index = builder.push(record);
            // A log written before the size was recorded carries zero, which
            // is an absent measurement rather than an empty recall.
            if *bytes > 0 {
                builder.set_input(index, format!("{} injected", fmt_bytes(*bytes)));
            }
            builder.set_output(index, keys.join("\n"));
        }
        SessionEvent::HookSignal {
            verdict,
            reason,
            hook_name,
            ..
        } => apply_hook_signal(builder, verdict, reason, hook_name, ev.ts),
        SessionEvent::TurnAborted { reason } => {
            builder.success = false;
            builder.push_signal(TrajectoryRecordKind::Error, None, reason, ev.ts);
        }
        SessionEvent::Summary { text } => {
            builder.push_signal(TrajectoryRecordKind::Compaction, None, text, ev.ts);
        }
        SessionEvent::RunCompleted { secs: Some(secs) } => {
            builder.run_completed_ms = (*secs as u64) * 1000;
        }
        _ => {}
    }
}

/// Fold a hook verdict into the turn.
///
/// A bare Allow is derivable from absence and carries no signal. A Deny is a
/// failure and is drawn as one. Every other verdict is a control-flow signal
/// that did not fail — an observation, an injection, a request for the user —
/// so it becomes a neutral hook record rather than a red error.
fn apply_hook_signal(
    builder: &mut TurnBuilder,
    verdict: &HookVerdictKind,
    reason: &str,
    hook_name: &str,
    ts: u64,
) {
    let kind = match verdict {
        HookVerdictKind::Allow => return,
        HookVerdictKind::Deny => TrajectoryRecordKind::Error,
        _ => TrajectoryRecordKind::Hook,
    };
    let name = if hook_name.is_empty() {
        "hook".to_string()
    } else {
        hook_name.to_string()
    };
    builder.touch(ts);
    let record = builder.record(
        kind,
        Some(name),
        format!("{verdict:?} {}", preview(reason)),
        ts,
    );
    builder.push(record);
}
