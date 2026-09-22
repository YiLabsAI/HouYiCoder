//! Trajectory view assembly from durable session events.
//!
//! Groups SessionLogEntry records into turns and builds the TrajectoryView
//! rendered by the /trajectory pane. Keeps log reads and turn grouping in the
//! CLI layer so the TUI stays a presentation-only consumer.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use houyicoder_api::session::SessionLog;
use houyicoder_context::{SessionEvent, SessionId, SessionLogEntry};
use houyicoder_tui::records::ToolOutcome;
#[path = "trajectory_turns.rs"]
mod turns;

use houyicoder_tui::view::trajectory_pane::{
    CompactedBoundary, EventTiming, EventUsage, ModelSwitchBoundary, RecordOutcome, SessionTiming,
    SubagentUsage, TrajectoryLog, TrajectoryRecord, TrajectoryRecordKind, TrajectoryRow,
    TrajectoryTurn, TrajectoryView, TurnBoundary,
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
fn spawned_call_ids(events: &[SessionLogEntry]) -> HashSet<&str> {
    let mut suppressed = HashSet::new();
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

fn build_summary(
    turns: Vec<TrajectoryTurn>,
    acc: &AccTotals,
    model: &str,
    hidden_turns: usize,
) -> TrajectoryView {
    // The session's economic account is what the whole session spent: the
    // parent's own calls plus every delegated child's. A child that reported no
    // usage leaves the total unknown rather than understated.
    let subagent_usage = (acc.subagent_calls > 0).then_some(SubagentUsage {
        calls: acc.subagent_calls,
        input: acc.subagent_input,
        output: acc.subagent_output,
        cache_read: acc.subagent_cache_read,
    });
    let subagent_unknown =
        subagent_usage.is_some_and(|d| d.input == 0 && d.output == 0 && d.cache_read == 0);
    // The totals are known when every turn reported usage, or when delegated
    // work accounted for the turns that did not. Otherwise they are unknown.
    let subagent_covered = acc.subagent_calls > 0;
    let any_unknown = (acc.usage_events == 0 || acc.usage_events < acc.turns) && !subagent_covered
        || subagent_unknown;
    let total_turns = turns.len() + hidden_turns;
    let duration_secs = acc.duration_ms / 1000;
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
    let total_cache_read = acc.cache_read + acc.subagent_cache_read;
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
    let rows = turns.into_iter().map(TrajectoryRow::Turn).collect();
    TrajectoryView {
        session_id: String::new(),
        model: header_model,
        total_turns,
        tokens_in: if any_unknown {
            None
        } else {
            Some((acc.total_in + acc.subagent_input) as usize)
        },
        tokens_out: if any_unknown {
            None
        } else {
            Some((acc.total_out + acc.subagent_output) as usize)
        },
        cache_read: if total_cache_read > 0 {
            Some(total_cache_read)
        } else {
            None
        },
        failures: acc.failures,
        duration_secs,
        timing: session_timing,
        hidden_turns,
        subagent_usage,
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
    subagent_calls: usize,
    subagent_input: u64,
    subagent_output: u64,
    subagent_cache_read: u64,
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
    let mut turn_rows: Vec<TrajectoryTurn> = Vec::new();
    let mut builder = turns::TurnBuilder::new();
    // Numbering continues from where the hidden turns left off, so the oldest
    // visible turn keeps the number it has in the whole session.
    let mut n: usize = hidden_turns;
    let mut acc = AccTotals::default();
    let calls = index_calls(window);
    let spawned = spawned_call_ids(window);
    let mut pending: Option<TurnBoundary> = None;
    let mut last_model: Option<String> = None;

    for ev in window {
        if turns::apply_turn_boundary(
            &mut builder,
            ev,
            &mut turn_rows,
            &mut n,
            &mut pending,
            &mut last_model,
        ) {
            continue;
        }
        turns::apply_turn_content(&mut builder, ev, &calls, &spawned);
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
    // Session-level totals: read once from the whole log, never from the page.
    accumulate_session(events, &mut acc);
    build_summary(turn_rows, &acc, model, hidden_turns)
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
                ..
            } => {
                acc.total_in += *input_tokens;
                acc.total_out += *output_tokens;
                acc.cache_read += *cache_read_input_tokens;
                acc.decode_tokens += *output_tokens;
                acc.usage_events += 1;
            }
            SessionEvent::UserInput { .. } => acc.turns += 1,
            // A delegated child runs its own provider calls, so its usage is
            // not in the parent's totals; it is reported on its own row.
            SessionEvent::SubagentReturn {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                ..
            } => {
                acc.subagent_calls += 1;
                acc.subagent_input += *input_tokens;
                acc.subagent_output += *output_tokens;
                acc.subagent_cache_read += *cache_read_input_tokens;
            }
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
fn fmt_bytes(bytes: u32) -> String {
    if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}
/// How many turns the pane loads by default, and how many it adds each time
/// the user asks for older history.
const TRAJECTORY_PAGE_TURNS: usize = 100;

/// The projection the pane last asked for, keyed by the durable revision it was
/// built from. The pane draws every frame, so re-projecting an unchanged log on
/// each draw would burn the whole log read and fold per frame for a screen that
/// is not changing.
struct CachedView {
    /// The last durable event id in the log the view was built from. Ids are
    /// monotonic, so an unchanged id means nothing was appended since.
    revision: Option<houyicoder_context::EventId>,
    /// How many turns the caller asked for when this view was built.
    max_turns: usize,
    view: TrajectoryView,
}

/// Session log trajectory reader: reads a session's durable log and returns the
/// current TrajectoryView on request, reusing the last projection while the log
/// has not changed.
pub struct SessionLogTrajectory {
    pub(crate) session_log: Arc<dyn SessionLog>,
    pub(crate) session_id: SessionId,
    pub(crate) model: String,
    cache: Mutex<Option<CachedView>>,
    /// Turns to load: one page at first, grown when the user walks past the
    /// oldest loaded turn.
    loaded_turns: AtomicUsize,
}

impl SessionLogTrajectory {
    pub fn new(session_log: Arc<dyn SessionLog>, session_id: SessionId, model: String) -> Self {
        Self {
            session_log,
            session_id,
            model,
            cache: Mutex::new(None),
            loaded_turns: AtomicUsize::new(TRAJECTORY_PAGE_TURNS),
        }
    }

    fn max_turns(&self) -> usize {
        self.loaded_turns.load(Ordering::Relaxed)
    }
}

impl TrajectoryLog for SessionLogTrajectory {
    fn trajectory(&self) -> TrajectoryView {
        let revision = self.session_log.last_trajectory_id(self.session_id);
        let max_turns = self.max_turns();
        if let Ok(cache) = self.cache.lock()
            && let Some(cached) = cache.as_ref()
            && cached.revision == revision
            && cached.max_turns == max_turns
        {
            return cached.view.clone();
        }
        let events = self.session_log.trajectory_snapshot(self.session_id);
        let view = project(&events, &self.model, max_turns);
        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some(CachedView {
                revision,
                max_turns,
                view: view.clone(),
            });
        }
        view
    }

    fn load_older(&self) {
        self.loaded_turns
            .fetch_add(TRAJECTORY_PAGE_TURNS, Ordering::Relaxed);
    }
}

#[cfg(test)]
#[path = "trajectory_view_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "trajectory_records_tests.rs"]
mod record_tests;

#[cfg(test)]
#[path = "trajectory_paging_tests.rs"]
mod paging_tests;
