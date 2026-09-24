//! The pane's own data types: what a row is, and what the pane renders.
//!
//! Plain data, so the composition root can build it and the render path can
//! read it without either depending on the other.

use crate::state::TrajectoryTurnKey;

use super::{EventTiming, EventUsage, RecordOutcome, TrajectoryRecordKind, TurnBoundary};

#[derive(Clone, PartialEq, Debug)]
pub struct TrajectoryRecord {
    pub kind: TrajectoryRecordKind,
    /// The record's own name: tool name, agent type, or model id. None when the
    /// log does not name it (an unnamed context row, an old record).
    pub name: Option<String>,
    /// The record's ordinal among the calls of its kind in this turn, so a
    /// multi-call turn reads as Model 1, Tool 2, Model 3 rather than as
    /// interchangeable rows. Zero when the kind is not numbered.
    pub ordinal: u32,
    pub summary: String,
    /// Offset from the turn start, in ms. Positions the event on the shared
    /// time axis so parallel events overlap on the same columns and sequence
    /// is visible at a glance — not just duration.
    pub start_ms: u64,
    pub duration_ms: u64,
    pub outcome: RecordOutcome,
    /// Full text for the Level 2 detail view: the model's thinking, a tool's
    /// input, a tool's result or the model's reply. Held separate from summary
    /// (the one-line L1 preview) so L2 shows full content without
    /// re-truncating.
    pub thinking: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
    /// Model calls only.
    pub usage: Option<EventUsage>,
    /// Model calls only.
    pub timing: Option<EventTiming>,
    /// Model calls only: length-recovery retries folded into this call.
    pub retries: usize,
}

#[derive(Clone, PartialEq, Debug)]
pub struct TrajectoryTurn {
    pub n: usize,
    /// The durable identity of the turn: the event that opened it. A turn
    /// number names a turn within one history; this names it across a clear,
    /// and it is what a detail read is asked for by.
    pub key: TrajectoryTurnKey,
    /// Boundaries the log recorded between the previous turn and this one, in
    /// the order they happened. Several durable facts can land in one gap (a
    /// compaction and then a model switch), so this is a list rather than a
    /// slot: a single slot would drop one of them without saying so. Data only;
    /// the pane decides how to draw them.
    pub boundary_before: Vec<TurnBoundary>,
    /// What the turn's row is titled: a preview of the prompt, or of the first
    /// record's summary for a turn whose prompt sits outside the loaded window.
    /// Derived where the records are, so the list needs no detail to name a row,
    /// and a preview because the row draws one line.
    pub title: String,
    pub tokens_in: Option<usize>,
    pub tokens_out: Option<usize>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    /// Every model id this turn called, in first-use order and de-duplicated.
    /// A turn can switch models mid-flight, so this is a list rather than a
    /// single id; empty when no TurnUsage landed or the log predates the field
    /// — unknown, not blank.
    pub models: Vec<String>,
    /// Every effort level this turn sent, in first-use order and
    /// de-duplicated. Empty when no effort parameter was sent (model
    /// unsupported, auto, or old log) — unknown, not auto.
    pub efforts: Vec<String>,
    /// Reasoning tokens across this turn's calls (a component of
    /// output_tokens, not a separate total). None when no TurnUsage landed or
    /// the log predates the field — unknown, not zero.
    pub reasoning_tokens: Option<usize>,
    pub tool_count: usize,
    pub tool_fail: usize,
    pub retries: usize,
    pub duration_ms: u64,
    pub success: bool,
}

#[derive(Clone, PartialEq, Debug)]
pub struct TrajectoryBg {
    pub kind: String,
    pub summary: String,
    pub duration_ms: u64,
}

#[derive(Clone)]
pub enum TrajectoryRow {
    Turn(TrajectoryTurn),
    Bg(TrajectoryBg),
}

/// Session-wide latency and work-time facts, computed once from the durable
/// timing events and read by both the trajectory pane and the status pane. One
/// value, two renderers: neither surface recomputes a percentile or a rate, so
/// they cannot disagree about the session they describe.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct SessionTiming {
    /// Time-to-first-token samples and their nearest-rank percentiles. None
    /// when the session recorded no first token at all.
    pub ttft_samples: usize,
    pub ttft_avg_ms: Option<u64>,
    pub ttft_p95_ms: Option<u64>,
    pub ttft_p99_ms: Option<u64>,
    /// Decode samples: how many calls reported a decode span.
    pub decode_samples: usize,
    pub decode_tok_per_sec: Option<f64>,
    /// Wall time spent inside model calls, and inside tool executions. The two
    /// can overlap (a delegation runs while its parent waits), so they are
    /// reported separately and never added into a single total.
    pub model_ms: u64,
    pub tool_ms: u64,
}

impl SessionTiming {
    /// True when the session recorded no timing at all, so a caller hides the
    /// rows rather than printing zeroes.
    pub fn is_empty(&self) -> bool {
        self.ttft_samples == 0
            && self.decode_samples == 0
            && self.model_ms == 0
            && self.tool_ms == 0
    }
}

/// What delegated sub-agents spent, summed over the session's delegations.
///
/// The session totals already include it: both the trajectory totals and the
/// status tally read the durable SubagentReturn boundaries. This type is the
/// breakdown, so a surface can say how much of the total the children
/// contributed instead of only reporting one undivided number.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct SubagentUsage {
    pub calls: usize,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
}

impl SubagentUsage {
    /// The share of the delegated input that came from cache, when any input
    /// was reported at all.
    pub fn cache_hit_pct(&self) -> Option<f64> {
        (self.input > 0).then(|| 100.0 * self.cache_read as f64 / self.input as f64)
    }
}

/// What the pane can say about its data right now.
///
/// The rows come from a bounded page read, so the first frame after a session
/// is opened has nothing to show yet. That is a state of its own: a pane that
/// rendered an empty list, or fell back to the demonstration rows, would tell
/// the user the session has no turns when the read simply has not landed.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum TrajectoryViewState {
    /// The first page is being read.
    Loading,
    /// A page is loaded and the rows below it are real.
    #[default]
    Ready,
    /// Older turns are being read; the rows already shown stay.
    LoadingOlder,
    /// The read failed, so there is nothing truthful to show.
    Failed,
}

#[derive(Clone, Default)]
pub struct TrajectoryView {
    pub session_id: String,
    /// Derived: one model when every turn's model field matches (or is
    /// None); "N models" when ≥2 distinct ids appear. Replaces the
    /// construction-time string snapshot so a mid-session model switch
    /// surfaces immediately.
    pub model: String,
    /// Distinct model ids the session used, so a row can decide whether to
    /// name its own model from a session fact rather than from the window.
    pub models_used: usize,
    pub total_turns: usize,
    pub tokens_in: Option<usize>,
    pub tokens_out: Option<usize>,
    pub cache_read: Option<u64>,
    pub failures: usize,
    /// Tool calls the session issued, from the session summary rather than
    /// from the rows the window happens to hold.
    pub tool_calls: usize,
    /// The session's wall time: the span between its first and last durable
    /// event, or None when the log carried no event to measure between.
    pub duration_ms: Option<u64>,
    pub timing: SessionTiming,
    /// How many turns sit before the loaded window. Non-zero means older
    /// history exists and has not been read yet.
    pub hidden_turns: usize,
    /// How many turns sit after the loaded window. Non-zero means the window
    /// has been walked back from the tail and newer turns are not loaded; End
    /// returns to them.
    pub newer_hidden: usize,
    /// Which history these rows belong to. It changes when a clear starts a
    /// new one, and the turn numbers of a new history name different turns,
    /// so a selection made under the old one must not be restored.
    pub history_generation: u64,
    /// What delegated sub-agents spent, when the session delegated any work.
    pub subagent_usage: Option<SubagentUsage>,
    /// Whether the rows below are loaded, still loading, or unavailable.
    pub state: TrajectoryViewState,
    /// Log lines in the loaded window that could not be read. The rest of the
    /// window is still shown; a surface that stayed silent about them would
    /// report a session as smaller than it is.
    pub skipped_records: usize,
    pub rows: Vec<TrajectoryRow>,
}
