//! The trajectory-data bridge: an impl of the TUI's TrajectoryLog seam backed
//! by the runner's SessionLog. The /trajectory pane queries the durable
//! session log (every TurnEvent) via SessionLog::trajectory_snapshot,
//! groups events into logical turns, and projects each into the plain-data
//! TrajectoryView the TUI renders. Mirrors the disk-search bridge: the TUI
//! owns the contract, this module owns the projection + the session-log
//! access, the TUI never touches the log file or the event types.
//!
//! The TUI is a synchronous render loop; SessionLog::trajectory_snapshot is
//! sync (in-memory, no I/O — it reads the live log buffer), so the bridge
//! does not need the async block_on the disk-search bridge uses for replay.

use std::collections::HashMap;
use std::sync::Arc;

use houyicoder_api::session::SessionLog;
use houyicoder_context::{SessionId, TurnEvent, TurnEventKind};
use houyicoder_tui::records::ToolOutcome;
use houyicoder_tui::view::trajectory_pane::{
    TrajectoryEvent, TrajectoryLog, TrajectoryRow, TrajectoryTurn, TrajectoryView,
};

/// Which tool a call id invoked, and with what input, so a later ToolResult
/// can be judged against the call that produced it. Built from the ToolCall
/// events, which always precede their result in append order.
type CallIndex = HashMap<String, (String, serde_json::Value)>;

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
fn index_calls(events: &[TurnEvent]) -> CallIndex {
    let mut calls = CallIndex::new();
    for ev in events {
        if let TurnEventKind::ToolCall {
            call_id,
            tool,
            input,
        } = &ev.kind
        {
            calls.insert(call_id.clone(), (tool.clone(), input.clone()));
        }
    }
    calls
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
        Some((t, i)) => (t.as_str(), i),
        // No matching call (a result whose call frame is outside this log
        // slice): judge on the output alone. from_output_with with an empty
        // tool name applies the plain error-or-success rule.
        None => ("", &serde_json::Value::Null),
    };
    ToolOutcome::from_output_with(output, tool, input) == ToolOutcome::Error
}

/// Project one TurnEvent into a TrajectoryEvent (the per-event row), or None
/// for kinds that are pure metadata (TurnUsage carries tokens at the turn
/// level, not as a displayable event; the rest are folded into the turn's
/// counts or skipped as audit-only).
fn project_event(ev: &TurnEvent, start_ms: u64, calls: &CallIndex) -> Option<TrajectoryEvent> {
    let success = !matches!(
        &ev.kind,
        TurnEventKind::ToolResult { output, call_id, .. }
            if result_failed(output, call_id, calls)
    );
    let (kind, summary, thinking, input, output, duration_ms) = match &ev.kind {
        TurnEventKind::UserInput { text } => {
            ("user", preview(text), None, Some(text.clone()), None, 0)
        }
        TurnEventKind::MidTurnInput { text } => {
            ("user", preview(text), None, Some(text.clone()), None, 0)
        }
        TurnEventKind::AssistantMessage { text, thinking } => {
            // output carries the full reply so the L2 detail shows it (the
            // summary is only an 80-char preview).
            (
                "llm",
                preview(text),
                thinking.clone(),
                None,
                Some(text.clone()),
                0,
            )
        }
        TurnEventKind::Reasoning { text } => (
            "reasoning",
            preview(text),
            Some(text.clone()),
            None,
            None,
            0,
        ),
        TurnEventKind::ToolCall { tool, input, .. } => (
            "tool_call",
            format!("{tool}({})", preview(&input.to_string())),
            None,
            Some(input.to_string()),
            None,
            0,
        ),
        TurnEventKind::ToolResult {
            output,
            duration_ms,
            ..
        } => {
            // Format the tool output the SAME way the transcript does — a
            // failed bash command shows "Exit code N" + stderr, an edit shows
            // its diff summary + body, an error shows "error: <msg>". Routing
            // the trajectory L2 through the same extract_body the transcript
            // uses means one rendering path for tool results, not two that
            // drift (the transcript got exit-code formatting; the trajectory
            // kept the raw JSON dump and showed {"error":"...","exit_code":1}
            // to the user on drill-down).
            let body = houyicoder_tui::result_body::extract_body(&output.to_string());
            (
                "tool_result",
                preview(&body),
                None,
                None,
                Some(body),
                *duration_ms,
            )
        }
        TurnEventKind::HookSignal {
            verdict, reason, ..
        } => ("hook", format!("{verdict:?} {reason}"), None, None, None, 0),
        TurnEventKind::TurnAborted { reason } => ("aborted", preview(reason), None, None, None, 0),
        TurnEventKind::Summary { text } => {
            ("summary", preview(text), None, Some(text.clone()), None, 0)
        }
        // Metadata / audit-only / streaming-delta: not a displayable event.
        // TurnStarted is the turn boundary (the projection groups on it); it
        // is not itself an event row. TurnUsage contributes tokens at the
        // turn level (not an event row).
        TurnEventKind::TurnStarted { .. }
        | TurnEventKind::TurnUsage { .. }
        | TurnEventKind::TruncationVerdict { .. }
        | TurnEventKind::PermissionDecision { .. }
        | TurnEventKind::CompactionBoundary { .. }
        | TurnEventKind::CacheBreak { .. }
        | TurnEventKind::MetaUser { .. }
        | TurnEventKind::MemoryRecall { .. }
        | TurnEventKind::AssistantTextDelta { .. }
        | TurnEventKind::WorktreeEnter { .. }
        | TurnEventKind::WorktreeExit { .. }
        | TurnEventKind::RewardObservation { .. } => return None,
        TurnEventKind::Unknown => return None,
    };
    Some(TrajectoryEvent {
        kind: kind.to_string(),
        summary,
        start_ms,
        duration_ms,
        success,
        thinking,
        input,
        output,
    })
}

/// Project the durable event stream into the trajectory view. Turns are
/// grouped on TurnStarted (the durable boundary appended at each model-call
/// entry) — NOT on UserInput: a single user prompt spans N tool-iteration
/// turns, and grouping on UserInput flattens them, hiding the per-iteration
/// work (retries, per-call tokens) the record layer spent two rounds
/// establishing. For old logs that predate TurnStarted, falls back to
/// UserInput grouping so they still render.
///
/// Per-turn tokens are Option: None when the turn had no TurnUsage (cancelled
/// or errored mid-stream) — unknown, not zero, per the unknown-must-be-None
/// rule. start_ms is offset from the turn's TurnStarted.ts (or UserInput.ts in
/// the legacy path), so a late iteration's bar sits at its real offset, not
/// "tens of seconds after the user typed."
/// Per-turn accumulator. Holds all the mutable state the projection loop
/// tracks for the current turn, so the loop body is a thin dispatch instead
/// of 17 inline field updates. reset is called at every turn boundary;
/// flush pushes the accumulated TrajectoryTurn + clears the event list.
struct TurnBuilder {
    events: Vec<TrajectoryEvent>,
    user_input: String,
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    model: Option<String>,
    effort: Option<String>,
    reasoning_tokens: Option<u64>,
    tool_count: usize,
    tool_fail: usize,
    retries: usize,
    duration_ms: u64,
    success: bool,
    turn_start_ts: Option<u64>,
}

impl TurnBuilder {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            user_input: String::new(),
            tokens_in: None,
            tokens_out: None,
            cache_read: None,
            cache_write: None,
            model: None,
            effort: None,
            reasoning_tokens: None,
            tool_count: 0,
            tool_fail: 0,
            retries: 0,
            duration_ms: 0,
            success: true,
            turn_start_ts: None,
        }
    }

    fn reset(&mut self, user_input: String, ts: u64) {
        self.events.clear();
        self.user_input = user_input;
        self.tokens_in = None;
        self.tokens_out = None;
        self.cache_read = None;
        self.cache_write = None;
        self.model = None;
        self.effort = None;
        self.reasoning_tokens = None;
        self.tool_count = 0;
        self.tool_fail = 0;
        self.retries = 0;
        self.duration_ms = 0;
        self.success = true;
        self.turn_start_ts = Some(ts);
    }

    fn flush(&mut self, turns: &mut Vec<TrajectoryTurn>, n: usize) {
        turns.push(TrajectoryTurn {
            n,
            user_input: std::mem::take(&mut self.user_input),
            tokens_in: self.tokens_in.map(|v| v as usize),
            tokens_out: self.tokens_out.map(|v| v as usize),
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            model: self.model.take(),
            effort: self.effort.take(),
            reasoning_tokens: self.reasoning_tokens.map(|v| v as usize),
            tool_count: self.tool_count,
            tool_fail: self.tool_fail,
            retries: self.retries,
            duration_ms: self.duration_ms,
            success: self.success,
            events: std::mem::take(&mut self.events),
        });
    }

    fn push_event(&mut self, ev: &TurnEvent, offset: u64, calls: &CallIndex) {
        if let Some(e) = project_event(ev, offset, calls) {
            self.events.push(e);
        }
    }

    fn offset(&self, ev_ts: u64) -> u64 {
        ev_ts.saturating_sub(self.turn_start_ts.unwrap_or(ev_ts))
    }
}

/// Build the TrajectoryView header from the accumulated turns: session
/// totals, model label (one id or "N models"), duration, failure count.
fn build_summary(
    turns: Vec<TrajectoryTurn>,
    total_tokens_in: u64,
    total_tokens_out: u64,
    total_failures: usize,
    model: &str,
) -> TrajectoryView {
    let any_unknown = turns.is_empty()
        || turns
            .iter()
            .any(|t| t.tokens_in.is_none() || t.tokens_out.is_none());
    let total_turns = turns.len();
    let duration_secs = turns.iter().map(|t| t.duration_ms).sum::<u64>() / 1000;
    let distinct_models: Vec<&str> = turns
        .iter()
        .filter_map(|t| t.model.as_deref())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let header_model = match distinct_models.len() {
        0 => model.to_string(),
        1 => distinct_models[0].to_string(),
        n => format!("{n} models"),
    };
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
        failures: total_failures,
        duration_secs,
        rows: turns.into_iter().map(TrajectoryRow::Turn).collect(),
    }
}

pub(crate) fn project(events: &[TurnEvent], model: &str) -> TrajectoryView {
    let has_turn_started = events
        .iter()
        .any(|e| matches!(e.kind, TurnEventKind::TurnStarted { .. }));

    let mut turns: Vec<TrajectoryTurn> = Vec::new();
    let mut builder = TurnBuilder::new();
    let mut pending_prompt = String::new();
    let mut n: usize = 0;
    let mut total_tokens_in: u64 = 0;
    let mut total_tokens_out: u64 = 0;
    let mut total_failures: usize = 0;
    let calls = index_calls(events);

    for ev in events {
        match &ev.kind {
            TurnEventKind::UserInput { text } if !has_turn_started => {
                if n > 0 {
                    builder.flush(&mut turns, n);
                }
                n += 1;
                builder.reset(text.clone(), ev.ts);
                builder.push_event(ev, 0, &calls);
            }
            TurnEventKind::UserInput { text } => {
                pending_prompt = text.clone();
            }
            TurnEventKind::TurnStarted { turn, .. } => {
                if n > 0 {
                    builder.flush(&mut turns, n);
                }
                n = *turn as usize;
                builder.reset(std::mem::take(&mut pending_prompt), ev.ts);
            }
            TurnEventKind::TurnUsage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_write_input_tokens,
                reasoning_tokens,
                model: ev_model,
                effort,
                recovery,
                ..
            } => {
                builder.tokens_in = Some(*input_tokens);
                builder.tokens_out = Some(*output_tokens);
                builder.cache_read = Some(*cache_read_input_tokens);
                builder.cache_write = Some(*cache_write_input_tokens);
                builder.model = if ev_model.is_empty() {
                    None
                } else {
                    Some(ev_model.clone())
                };
                builder.effort = effort.clone();
                builder.reasoning_tokens = Some(*reasoning_tokens);
                if *recovery {
                    builder.retries += 1;
                }
                total_tokens_in += *input_tokens;
                total_tokens_out += *output_tokens;
            }
            TurnEventKind::ToolCall { .. } => {
                builder.tool_count += 1;
                builder.push_event(ev, builder.offset(ev.ts), &calls);
            }
            TurnEventKind::ToolResult {
                duration_ms,
                output,
                call_id,
            } => {
                builder.duration_ms += *duration_ms;
                if result_failed(output, call_id, &calls) {
                    builder.tool_fail += 1;
                    total_failures += 1;
                }
                builder.push_event(ev, builder.offset(ev.ts), &calls);
            }
            TurnEventKind::TurnAborted { .. } => {
                builder.success = false;
                builder.push_event(ev, builder.offset(ev.ts), &calls);
            }
            _ => {
                builder.push_event(ev, builder.offset(ev.ts), &calls);
            }
        }
    }
    if n > 0 {
        builder.flush(&mut turns, n);
    }
    build_summary(
        turns,
        total_tokens_in,
        total_tokens_out,
        total_failures,
        model,
    )
}

/// The TrajectoryLog bridge: holds the runner's SessionLog + the session id +
/// the model name. trajectory() reads the live event buffer + projects.
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
#[path = "trajectory_bridge_tests.rs"]
mod tests;
