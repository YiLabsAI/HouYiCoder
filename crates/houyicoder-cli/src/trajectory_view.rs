//! Trajectory view assembly from durable session events.
//!
//! Groups SessionLogEntry records into turns and builds the TrajectoryView
//! rendered by the /trajectory pane. Keeps log reads and turn grouping in the
//! CLI layer so the TUI stays a presentation-only consumer.

use std::collections::HashMap;
use std::sync::Arc;

use houyicoder_api::session::SessionLog;
use houyicoder_context::{SessionEvent, SessionId, SessionLogEntry};
use houyicoder_tui::records::ToolOutcome;
#[path = "trajectory_turns.rs"]
mod turns;

use houyicoder_tui::view::trajectory_pane::{
    EventTiming, EventUsage, RecordOutcome, TrajectoryLog, TrajectoryRecord, TrajectoryRecordKind,
    TrajectoryRow, TrajectoryTurn, TrajectoryView, TurnBoundary,
};

/// Which tool a call id invoked, and with what input, so a later ToolResult
/// can be judged against the call that produced it. Built from the ToolCall
/// events, which always precede their result in append order.
///
/// Borrowed from the event slice rather than owned: a tool input can be the
/// whole payload the model sent (a write call carries the entire file body),
/// and the index exists only during view assembly, so copying
/// them would duplicate the session's writes for no gain.
type CallIndex<'a> = HashMap<&'a str, (&'a str, &'a serde_json::Value)>;

/// One line of preview text for an event (truncated so the L1 row stays one
/// line). The L2 detail carries the full content separately.
fn preview(s: &str) -> String {
    const MAX: usize = 80;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(MAX).collect();
        out.push('…');
        out
    }
}

/// Index every tool call in the stream by its call id. Done in a pass of its
/// own rather than while walking the turns: a result is judged against the
/// call that produced it, and the two events need not sit in the same turn,
/// so the index must be complete before the first result is judged.
fn index_calls(events: &[SessionLogEntry]) -> CallIndex<'_> {
    let mut calls = CallIndex::new();
    for ev in events {
        if let SessionEvent::ToolCall {
            call_id,
            tool,
            input,
        } = &ev.event
        {
            calls.insert(call_id.as_str(), (tool.as_str(), input));
        }
    }
    calls
}

/// The tool a model delegation is issued through.
const DELEGATION_TOOL: &str = "agent";

/// The tool calls that a delegation was spawned from, by call id.
///
/// A model delegation is issued as a tool call, and the spawn records that call
/// as its trigger. The turn shows one Agent record for the delegation, so the
/// underlying tool call must not also appear as a Tool record: the same work
/// counted twice, once as the mechanism and once as the delegation.
///
/// A spawn written before the trigger field existed carries an empty source,
/// which a replay reads as a model trigger. Those spawns are matched by
/// position instead: a spawn immediately follows the call it came from, so it
/// claims the newest delegation call not already claimed by another spawn.
fn spawned_call_ids(events: &[SessionLogEntry]) -> std::collections::HashSet<&str> {
    let mut suppressed = std::collections::HashSet::new();
    let mut unclaimed: Vec<&str> = Vec::new();
    for ev in events {
        match &ev.event {
            SessionEvent::ToolCall { call_id, tool, .. } if tool == DELEGATION_TOOL => {
                unclaimed.push(call_id.as_str());
            }
            SessionEvent::SubagentSpawn { trigger_source, .. } => {
                match trigger_source.strip_prefix("model:") {
                    Some(id) => {
                        suppressed.insert(id);
                        unclaimed.retain(|c| *c != id);
                    }
                    None => {
                        if let Some(id) = unclaimed.pop() {
                            suppressed.insert(id);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    suppressed
}

/// Whether a tool result records a failure, decided by the same judgment the
/// transcript chip uses.
///
/// The pane and the transcript describe one event, so they must not reach
/// opposite verdicts about it. Testing only for an error key does: a shell
/// command that exits non-zero reports the failure in exit_code and success,
/// and carries no error key at all (that key marks a tool-infrastructure
/// failure, not a command that ran and failed). Under the error-key test a
/// failed command was counted as a success here while the transcript painted
/// it red, and the pane's failure total could read zero for a session in
/// which every command failed. Routing through ToolOutcome also carries the
/// semantic-exit exception, so grep finding no matches stays a success in
/// both places.
fn result_failed(output: &serde_json::Value, call_id: &str, calls: &CallIndex) -> bool {
    let (tool, input) = match calls.get(call_id) {
        Some(&(t, i)) => (t, i),
        // No matching call (a result whose call frame is outside this log
        // slice): judge on the output alone. from_output_with with an empty
        // tool name applies the plain error-or-success rule.
        None => ("", &serde_json::Value::Null),
    };
    ToolOutcome::from_output_with(output, tool, input) == ToolOutcome::Error
}

/// Session-wide timing facts folded from the durable timing events.
struct TimingStats<'a> {
    ttfts: &'a [u64],
    decode_tokens: u64,
    decode_ms: u64,
}

fn build_summary(
    turns: Vec<TrajectoryTurn>,
    total_tokens_in: u64,
    total_tokens_out: u64,
    total_failures: usize,
    model: &str,
    timing: TimingStats<'_>,
) -> TrajectoryView {
    let any_unknown = turns.is_empty()
        || turns
            .iter()
            .any(|t| t.tokens_in.is_none() || t.tokens_out.is_none());
    let total_turns = turns.len();
    let duration_secs = turns.iter().map(|t| t.duration_ms).sum::<u64>() / 1000;
    let distinct_models: Vec<&str> = turns
        .iter()
        .flat_map(|t| t.models.iter().map(String::as_str))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let header_model = match distinct_models.len() {
        0 => model.to_string(),
        1 => distinct_models[0].to_string(),
        n => format!("{n} models"),
    };
    let total_cache_read: u64 = turns.iter().filter_map(|t| t.cache_read).sum();
    let (ttft_avg_ms, ttft_p95_ms, ttft_p99_ms) = if timing.ttfts.is_empty() {
        (None, None, None)
    } else {
        let mut sorted = timing.ttfts.to_vec();
        sorted.sort_unstable();
        let avg = sorted.iter().sum::<u64>() / sorted.len() as u64;
        let p95_idx = ((sorted.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
        let p99_idx = ((sorted.len() as f64 * 0.99).ceil() as usize).saturating_sub(1);
        (Some(avg), Some(sorted[p95_idx]), Some(sorted[p99_idx]))
    };
    let decode_tok_per_sec = if timing.decode_ms > 0 && timing.decode_tokens > 0 {
        Some(timing.decode_tokens as f64 / (timing.decode_ms as f64 / 1000.0))
    } else {
        None
    };
    let rows = turns.into_iter().map(TrajectoryRow::Turn).collect();
    TrajectoryView {
        session_id: String::new(),
        model: header_model,
        total_turns,
        tokens_in: if any_unknown {
            None
        } else {
            Some(total_tokens_in as usize)
        },
        tokens_out: if any_unknown {
            None
        } else {
            Some(total_tokens_out as usize)
        },
        cache_read: if total_cache_read > 0 {
            Some(total_cache_read)
        } else {
            None
        },
        failures: total_failures,
        duration_secs,
        ttft_avg_ms,
        ttft_p95_ms,
        ttft_p99_ms,
        decode_tok_per_sec,
        rows,
    }
}

/// Session-wide accumulator: token totals for the header plus the timing
/// samples the header percentiles are computed from.
struct AccTotals {
    total_in: u64,
    total_out: u64,
    failures: usize,
    ttfts: Vec<u64>,
    decode_tokens: u64,
    decode_ms: u64,
}

fn process_event_timing(ev: &SessionEvent, acc: &mut AccTotals) {
    if let SessionEvent::ModelStepTiming {
        ttft_ms,
        decode_ms: d_ms,
        ..
    } = ev
    {
        if let Some(ttft) = ttft_ms {
            acc.ttfts.push(*ttft);
        }
        if let Some(dec) = d_ms {
            acc.decode_ms += *dec;
        }
    }
}

/// Assemble the trajectory view from the durable event stream.
pub(crate) fn project(events: &[SessionLogEntry], model: &str) -> TrajectoryView {
    let mut turn_rows: Vec<TrajectoryTurn> = Vec::new();
    let mut builder = turns::TurnBuilder::new();
    let mut n: usize = 0;
    let mut acc = AccTotals {
        total_in: 0,
        total_out: 0,
        failures: 0,
        ttfts: Vec::new(),
        decode_tokens: 0,
        decode_ms: 0,
    };
    let calls = index_calls(events);
    let spawned = spawned_call_ids(events);
    let mut pending: Option<TurnBoundary> = None;

    for ev in events {
        process_event_timing(&ev.event, &mut acc);
        if turns::apply_turn_boundary(&mut builder, ev, &mut turn_rows, &mut n, &mut pending) {
            continue;
        }
        turns::apply_turn_content(&mut builder, ev, &calls, &spawned, &mut acc);
    }
    if builder.is_open() {
        // A turn opened by a non-boundary event (a windowed read that starts
        // mid-run) still needs a number; the boundary events are the only
        // other place one is assigned.
        if n == 0 {
            n = 1;
        }
        builder.flush(&mut turn_rows, n);
    }
    let timing = TimingStats {
        ttfts: &acc.ttfts,
        decode_tokens: acc.decode_tokens,
        decode_ms: acc.decode_ms,
    };
    build_summary(
        turn_rows,
        acc.total_in,
        acc.total_out,
        acc.failures,
        model,
        timing,
    )
}

/// A byte count as a compact string for the memory row.
fn fmt_bytes(bytes: u32) -> String {
    if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}
/// Session log trajectory reader: reads a session's durable log and returns
/// the current TrajectoryView on request.
pub struct SessionLogTrajectory {
    pub(crate) session_log: Arc<dyn SessionLog>,
    pub(crate) session_id: SessionId,
    pub(crate) model: String,
}

impl SessionLogTrajectory {
    pub fn new(session_log: Arc<dyn SessionLog>, session_id: SessionId, model: String) -> Self {
        Self {
            session_log,
            session_id,
            model,
        }
    }
}

impl TrajectoryLog for SessionLogTrajectory {
    fn trajectory(&self) -> TrajectoryView {
        let events = self.session_log.trajectory_snapshot(self.session_id);
        project(&events, &self.model)
    }
}

#[cfg(test)]
#[path = "trajectory_view_tests.rs"]
mod tests;
