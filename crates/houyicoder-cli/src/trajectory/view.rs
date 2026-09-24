//! Trajectory view assembly from durable session events.
//!
//! Groups SessionLogEntry records into turns and builds the TrajectoryView
//! rendered by the /trajectory pane. Keeps log reads and turn grouping in the
//! CLI layer so the TUI stays a presentation-only consumer.

use std::collections::{HashMap, HashSet};

use super::turns::{self, FoldMode};

use houyicoder_context::{SessionEvent, SessionLogEntry};
use houyicoder_core::agent::multi_agent::aggregate_subagent_usage;
use houyicoder_tui::state::TrajectoryTurnKey;
use houyicoder_tui::view::trajectory_pane::{
    SessionTiming, SubagentUsage, TrajectoryRecord, TrajectoryRow, TrajectoryTurn, TrajectoryView,
    TrajectoryViewState, TurnBoundary,
};

/// Which tool a call id invoked, and with what input, so a later ToolResult
/// can be judged against the call that produced it. Built from the ToolCall
/// events, which always precede their result in append order.
///
/// Borrowed from the event slice rather than owned: a tool input can be the
/// whole payload the model sent (a write call carries the entire file body),
/// and the index exists only during view assembly, so copying
/// them would duplicate the session's writes for no gain.
pub(super) type CallIndex = HashMap<String, (String, serde_json::Value)>;

/// One line of preview text for an event (truncated so the L1 row stays one
/// line). The L2 detail carries the full content separately.
pub(super) fn preview(s: &str) -> String {
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
fn index_calls(events: &[SessionLogEntry]) -> CallIndex {
    let mut calls = CallIndex::new();
    for ev in events {
        note_call(&mut calls, ev);
    }
    calls
}

/// Note a tool call in the index, so a later result can name the tool it
/// answers and the input it was given.
fn note_call(calls: &mut CallIndex, ev: &SessionLogEntry) {
    if let SessionEvent::ToolCall {
        call_id,
        tool,
        input,
    } = &ev.event
    {
        calls.insert(call_id.clone(), (tool.clone(), input.clone()));
    }
}

/// Note a delegation's trigger call.
///
/// A spawn claims the delegation call it was issued from, so the row that call
/// opened is not also drawn as a plain tool call. The unclaimed stack carries
/// across events: a spawn answers a call that may sit in an earlier one.
fn note_spawned(
    spawned: &mut HashSet<String>,
    unclaimed: &mut Vec<String>,
    ev: &SessionLogEntry,
) -> Option<String> {
    match &ev.event {
        SessionEvent::ToolCall { call_id, tool, .. } if tool == DELEGATION_TOOL => {
            unclaimed.push(call_id.clone());
            None
        }
        SessionEvent::SubagentSpawn { trigger_source, .. } => {
            let claimed = match trigger_source.strip_prefix("model:") {
                Some(id) => Some(id.to_string()),
                None => unclaimed.pop(),
            };
            if let Some(id) = claimed.as_ref() {
                spawned.insert(id.clone());
                unclaimed.retain(|c| c != id);
            }
            claimed
        }
        _ => None,
    }
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
///
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
/// which every command failed. The shared rule also carries the semantic-exit
/// exception, so grep finding no matches stays a success in both places.
pub(super) fn result_failed(output: &serde_json::Value, call_id: &str, calls: &CallIndex) -> bool {
    let null = serde_json::Value::Null;
    let (tool, input) = match calls.get(call_id) {
        Some((t, i)) => (t.as_str(), i),
        // No matching call (a result whose call frame is outside this log
        // slice): judge on the output alone, which the shared rule reads as
        // the plain error-or-success case.
        None => ("", &null),
    };
    houyicoder_protocol::tool::tool_result_failed(output, tool, input)
}

fn build_summary(
    rows: Vec<TrajectoryRow>,
    acc: &AccTotals,
    model: &str,
    hidden_turns: usize,
) -> TrajectoryView {
    // The session's economic account is what the whole session spent: the
    // parent's own calls plus every delegated child's. A child that reported no
    // usage leaves the total unknown rather than understated.
    let subagent_usage = (acc.subagent.calls > 0).then_some(acc.subagent);
    // One child that never reported its usage leaves a hole in the sum, so the
    // session total is unknown rather than short by that child.
    let subagent_unknown = acc.subagent_unmeasured;
    // The totals are known only when every turn reported usage and no delegation
    // left its own usage unmeasured. A turn with no usage is a hole in the sum,
    // and a child's tokens do not fill it: they are the child's own spend.
    let any_unknown = acc.usage_events == 0 || acc.usage_events < acc.turns || subagent_unknown;
    let total_turns = rows.len() + hidden_turns;
    let duration_secs = acc.duration_ms / 1000;
    let distinct_models: Vec<&str> = acc.models.iter().map(String::as_str).collect();
    let header_model = match distinct_models.len() {
        0 => model.to_string(),
        1 => distinct_models[0].to_string(),
        n => format!("{n} models"),
    };
    let total_cache_read = acc.cache_read + acc.subagent.cache_read;
    let (ttft_avg_ms, ttft_p95_ms, ttft_p99_ms) = if acc.ttfts.is_empty() {
        (None, None, None)
    } else {
        let mut sorted = acc.ttfts.clone();
        sorted.sort_unstable();
        let avg = sorted.iter().sum::<u64>() / sorted.len() as u64;
        let p95_idx = ((sorted.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
        let p99_idx = ((sorted.len() as f64 * 0.99).ceil() as usize).saturating_sub(1);
        (Some(avg), Some(sorted[p95_idx]), Some(sorted[p99_idx]))
    };
    let decode_tok_per_sec = if acc.decode_ms > 0 && acc.decode_tokens > 0 {
        Some(acc.decode_tokens as f64 / (acc.decode_ms as f64 / 1000.0))
    } else {
        None
    };
    // One typed summary for every surface that reports session latency.
    let session_timing = SessionTiming {
        ttft_samples: acc.ttfts.len(),
        ttft_avg_ms,
        ttft_p95_ms,
        ttft_p99_ms,
        decode_samples: acc.decode_samples,
        decode_tok_per_sec,
        model_ms: acc.model_ms,
        tool_ms: acc.tool_ms,
    };

    TrajectoryView {
        session_id: String::new(),
        model: header_model,
        total_turns,
        models_used: acc.models.len(),
        tokens_in: if any_unknown {
            None
        } else {
            Some((acc.total_in + acc.subagent.input) as usize)
        },
        tokens_out: if any_unknown {
            None
        } else {
            Some((acc.total_out + acc.subagent.output) as usize)
        },
        cache_read: if total_cache_read > 0 {
            Some(total_cache_read)
        } else {
            None
        },
        failures: acc.failures,
        tool_calls: acc.tool_calls,
        duration_secs,
        timing: session_timing,
        hidden_turns,
        // A whole-log projection ends at the newest turn, so nothing newer is
        // held back.
        newer_hidden: 0,
        history_generation: 0,
        subagent_usage,
        state: TrajectoryViewState::Ready,
        skipped_records: 0,
        rows,
    }
}

/// Session-wide accumulator: token totals for the header plus the timing
/// samples the header percentiles are computed from.
#[derive(Default)]
struct AccTotals {
    total_in: u64,
    total_out: u64,
    failures: usize,
    tool_calls: usize,
    /// Model ids the whole log used, so the header names the session's models
    /// rather than the window's.
    models: HashSet<String>,
    ttfts: Vec<u64>,
    decode_samples: usize,
    decode_tokens: u64,
    decode_ms: u64,
    model_ms: u64,
    tool_ms: u64,
    cache_read: u64,
    duration_ms: u64,
    /// Whole-log counts, so a completeness check cannot be fooled by the page.
    usage_events: usize,
    turns: usize,
    /// What the session's delegated children spent, from the shared aggregator.
    subagent: SubagentUsage,
    /// True when a child reached a terminal without reporting usage, so the
    /// session total is a lower bound rather than a complete figure.
    subagent_unmeasured: bool,
}

/// Fold one timing event into the session's latency samples. Tool durations are
/// counted where the failures are, so they are not added here as well.
fn process_event_timing(ev: &SessionEvent, acc: &mut AccTotals) {
    if let SessionEvent::ModelStepTiming {
        total_ms,
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
            acc.decode_samples += 1;
        }
        acc.model_ms += *total_ms;
    }
}

/// Assemble the trajectory view from the durable event stream.
///
/// The rows fold over the tail window only, so a long session costs the same to
/// draw as a short one. The session totals are a different question and are
/// answered by a pass over the whole log: the header reports what the session
/// spent, not what the visible page spent, and the status pane reads the same
/// whole-session figure.
pub(crate) fn project(events: &[SessionLogEntry], model: &str, max_turns: usize) -> TrajectoryView {
    let (window, first_turn) = tail_window(events, max_turns);
    let hidden_turns = first_turn.saturating_sub(1);
    let rows = project_rows(window, first_turn);
    // Session-level totals: read once from the whole log, never from the page.
    let mut acc = AccTotals::default();
    accumulate_session(events, &mut acc);
    build_summary(rows, &acc, model, hidden_turns)
}

/// Fold a slice of events into rows, numbering the first turn as first_turn.
///
/// A windowed read starts mid-session, so the number its oldest turn carries
/// comes from the caller rather than from the slice: numbering from the slice
/// would restart every page at the session's first turn.
pub(crate) fn project_rows(events: &[SessionLogEntry], first_turn: usize) -> Vec<TrajectoryRow> {
    fold_rows(events, first_turn, FoldMode::Summary).0
}

/// Fold a slice of events into rows, and each turn's records by key.
///
/// The mode decides what a record keeps: a paged read draws one line per turn
/// and keeps summaries, and a drill keeps the bodies it shows.
pub(crate) fn fold_rows(
    events: &[SessionLogEntry],
    first_turn: usize,
    mode: FoldMode,
) -> (
    Vec<TrajectoryRow>,
    Vec<(TrajectoryTurnKey, Vec<TrajectoryRecord>)>,
) {
    let mut projection = PageProjection::seed(events, first_turn, mode);
    let rows = projection.finish();
    (rows, projection.into_records())
}

/// The rows a page projected, and the state a later read extends them with.
///
/// A page is folded once, when it lands. An append extends the page that
/// received it by applying the new events to this state, so the turns already
/// closed are never folded again: what an append costs is the events it
/// brought, not the page it lands in.
pub(crate) struct PageProjection {
    /// The turns already closed, in order.
    turns: Vec<TrajectoryTurn>,
    /// The turn still being appended to, if any.
    open: turns::TurnBuilder,
    /// The number the next turn opened here carries.
    next_number: usize,
    pending_boundaries: Vec<TurnBoundary>,
    last_model: Option<String>,
    /// The tool calls this page has seen, and the delegations they spawned.
    ///
    /// Carried rather than rebuilt: a read's tool result names a call that may
    /// sit in an event the page read earlier.
    calls: CallIndex,
    spawned: HashSet<String>,
    /// Delegation calls a spawn has not claimed yet, in the order they were
    /// issued.
    unclaimed: Vec<String>,
    /// Each closed turn's records, for a fold that keeps them.
    records: Vec<(TrajectoryTurnKey, Vec<TrajectoryRecord>)>,
}

impl PageProjection {
    /// Fold a page's events into rows, numbering the first turn as first_turn.
    pub(crate) fn seed(events: &[SessionLogEntry], first_turn: usize, mode: FoldMode) -> Self {
        let mut projection = Self {
            turns: Vec::new(),
            open: turns::TurnBuilder::new(mode),
            next_number: first_turn.saturating_sub(1),
            pending_boundaries: Vec::new(),
            last_model: None,
            calls: CallIndex::new(),
            spawned: HashSet::new(),
            unclaimed: Vec::new(),
            records: Vec::new(),
        };
        projection.apply(events);
        projection
    }

    /// Apply the events a read brought, extending what this page already holds.
    ///
    /// The batch is indexed before it is folded. A delegation's trigger call is
    /// drawn as the delegation rather than as a plain tool call, and only the
    /// spawn that answers it says so: indexing as the fold went would draw the
    /// call before the spawn that claims it had been seen.
    pub(crate) fn apply(&mut self, events: &[SessionLogEntry]) {
        for ev in events {
            note_call(&mut self.calls, ev);
            if let Some(claimed) = note_spawned(&mut self.spawned, &mut self.unclaimed, ev) {
                // The call the spawn answers may sit in an earlier batch, where
                // it was already folded: its record is dropped so the turn does
                // not draw the same work twice.
                self.open.drop_tool(&claimed);
            }
        }
        for ev in events {
            if turns::dispatch::apply_turn_boundary(
                &mut self.open,
                ev,
                &mut self.turns,
                &mut self.next_number,
                &mut self.pending_boundaries,
                &mut self.last_model,
                &mut self.records,
            ) {
                continue;
            }
            // A page can start mid-run, where the first event is content: the
            // turn it belongs to is opened here, so it is numbered and it takes
            // that event's id as its identity.
            if !self.open.is_open() {
                self.next_number += 1;
                let boundaries = std::mem::take(&mut self.pending_boundaries);
                self.open.open(ev, boundaries);
            }
            turns::dispatch::apply_turn_content(&mut self.open, ev, &self.calls, &self.spawned);
        }
    }

    /// The rows as they stand, with the open turn's own row last.
    pub(crate) fn rows(&self) -> Vec<TrajectoryRow> {
        let mut rows: Vec<TrajectoryRow> = self
            .turns
            .iter()
            .cloned()
            .map(TrajectoryRow::Turn)
            .collect();
        if let Some(turn) = self.open.snapshot(self.next_number) {
            rows.push(TrajectoryRow::Turn(turn));
        }
        rows
    }

    /// Close the open turn, for a fold nothing will append to.
    pub(crate) fn finish(&mut self) -> Vec<TrajectoryRow> {
        if let Some((turn, key, records)) = self.open.finalize(self.next_number) {
            self.turns.push(turn);
            self.records.push((key, records));
        }
        self.rows()
    }

    /// The records this fold kept, by key.
    pub(crate) fn into_records(self) -> Vec<(TrajectoryTurnKey, Vec<TrajectoryRecord>)> {
        self.records
    }
}

/// One turn's records, folded from the whole log: what a drill asks for, in a
/// test that reads a turn's records directly.
#[cfg(test)]
pub(crate) fn records_of(events: &[SessionLogEntry], turn: usize) -> Vec<TrajectoryRecord> {
    fold_rows(events, 1, FoldMode::Records)
        .1
        .into_iter()
        .nth(turn.saturating_sub(1))
        .map(|(_, records)| records)
        .unwrap_or_default()
}

/// Fold the whole log's token, failure, duration, and timing totals. This is
/// the session-level figure every surface reports; the windowed rows answer a
/// different question and must not feed it.
fn accumulate_session(events: &[SessionLogEntry], acc: &mut AccTotals) {
    let calls = index_calls(events);
    for ev in events {
        process_event_timing(&ev.event, acc);
        match &ev.event {
            SessionEvent::TurnUsage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                model,
                ..
            } => {
                acc.total_in += *input_tokens;
                acc.total_out += *output_tokens;
                acc.cache_read += *cache_read_input_tokens;
                acc.decode_tokens += *output_tokens;
                acc.usage_events += 1;
                if !model.is_empty() {
                    acc.models.insert(model.clone());
                }
            }
            SessionEvent::UserInput { .. } => acc.turns += 1,
            SessionEvent::ToolCall { .. } => acc.tool_calls += 1,
            SessionEvent::ToolResult {
                output,
                call_id,
                duration_ms,
            } => {
                if result_failed(output, call_id, &calls) {
                    acc.failures += 1;
                }
                acc.tool_ms += *duration_ms;
            }
            _ => {}
        }
    }
    // Delegated usage comes from the one aggregator every surface shares, so
    // the trajectory totals and the status tally cannot drift apart.
    let delegated = aggregate_subagent_usage(events);
    acc.subagent = SubagentUsage {
        calls: delegated.calls,
        input: delegated.input_tokens,
        output: delegated.output_tokens,
        cache_read: delegated.cache_read_input_tokens,
    };
    acc.subagent_unmeasured = delegated.unmeasured_calls > 0;
    // The session's wall time is the span of its own durable events, which is
    // what the user waited, and it does not depend on which page is loaded.
    if let (Some(first), Some(last)) = (events.first(), events.last()) {
        acc.duration_ms = last.ts.saturating_sub(first.ts);
    }
}

/// The newest turns of a log, and the number the first of them carries.
///
/// A turn starts at a user input, so the window starts at the user input that
/// opens the oldest turn still shown. Anything before it belongs to an older
/// turn and is not read at all. max_turns of 0 means no limit.
///
/// A log that opens mid-run has model calls before its first user input, which
/// the fold treats as a turn of its own. That turn counts here too, or the
/// window would misnumber everything after it and under-report what it hid.
fn tail_window(events: &[SessionLogEntry], max_turns: usize) -> (&[SessionLogEntry], usize) {
    // A log that opens mid-run has model calls before its first user input,
    // which the fold treats as a turn of its own. It counts as a hidden turn
    // only when the window cuts it off, which it does whenever the window
    // starts at a user input.
    let leading_turn = usize::from(
        events
            .first()
            .is_some_and(|e| !matches!(e.event, SessionEvent::UserInput { .. })),
    );
    let starts: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e.event, SessionEvent::UserInput { .. }))
        .map(|(i, _)| i)
        .collect();
    if max_turns == 0 || starts.len() <= max_turns {
        // Nothing is cut, so the window holds the session's first turn.
        return (events, 1);
    }
    let cut = starts[starts.len() - max_turns];
    let hidden = starts.len() - max_turns + leading_turn;
    (&events[cut..], hidden + 1)
}

/// A byte count as a compact string for the memory row.
pub(super) fn fmt_bytes(bytes: u32) -> String {
    if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}
