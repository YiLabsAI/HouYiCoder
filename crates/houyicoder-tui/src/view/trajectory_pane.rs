//! The /trajectory pane: a turn-organized distributed-trace view with a
//! 3-level drill-down and an ASCII time axis. Follows the /memory
//! /permissions pane shape (shared draw_command_pane template) but
//! renders a session timeline. The view types live here; the drill-down
//! renderers live in the detail submodule.
//!
//! 3 levels:
//! - Level 0: session summary + turn list (collapsed, cursor selects)
//! - Level 1: turn detail — record timeline + ASCII bar (cursor selects)
//! - Level 2: record detail — full data (long content folded)
//!
//! Keys: Up/Down move cursor at current level, Enter drills down,
//! Esc goes back one level (or closes at level 0).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::view::line_wrap::truncate_width;
use crate::view::navigation::key_hint;
use crate::view::relative_time::{now_epoch_secs, relative_time};
use std::collections::HashSet;
use unicode_width::UnicodeWidthStr;

// Data types

/// What one timeline record represents. These are domain names: a record is a
/// model call, a tool call, a piece of context, or an agent, not a raw log
/// variant and not a provider stream detail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrajectoryRecordKind {
    /// User input, a mid-turn update, or context injected by the runner.
    Context,
    /// One model call: thinking, response, usage, and timing.
    Model,
    /// One tool call together with its result.
    Tool,
    /// A delegated sub-agent, spanning spawn to return.
    Agent,
    /// Memory recalled into this turn.
    Memory,
    /// A hook verdict that did not deny: an observation, an injection, or a
    /// request for the user. Not a failure, so it is not drawn as one.
    Hook,
    /// Context folded away by compaction.
    Compaction,
    /// A turn-level failure or interruption marker.
    Error,
}

impl TrajectoryRecordKind {
    /// The kind column label. Fixed width so the columns to its right line up.
    pub fn label(self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::Model => "model",
            Self::Tool => "tool",
            Self::Agent => "agent",
            Self::Memory => "memory",
            Self::Hook => "hook",
            Self::Compaction => "compact",
            Self::Error => "error",
        }
    }
}

/// Token facts for one model call. Every field is optional: a provider that
/// omits usage leaves the value unknown, never zero.
#[derive(Clone, Copy, Default)]
pub struct EventUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
}

/// Latency facts for one model call, as measured by the agent loop. A call
/// that produced no first token (aborted or failed) leaves the split unknown
/// while still carrying its total wall time.
#[derive(Clone, Copy, Default)]
pub struct EventTiming {
    pub total_ms: u64,
    pub ttft_ms: Option<u64>,
    pub decode_ms: Option<u64>,
}

/// How a record ended. Three states, not a boolean: a tool call whose result
/// has not landed yet is not a success, and claiming it succeeded would put a
/// checkmark on work that may still fail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordOutcome {
    Ok,
    Failed,
    /// Started but not finished inside the loaded window.
    Pending,
}

impl RecordOutcome {
    /// The status glyph the timeline and the detail header show.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Ok => "✓",
            Self::Failed => "✗",
            Self::Pending => "…",
        }
    }
}

/// A durable boundary recorded between two turns.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TurnBoundary {
    /// The user cleared the conversation context before this turn. prior_turn
    /// is the model-call count at the clear; at_secs is the durable event's
    /// timestamp, so the pane can say when it happened.
    ContextCleared { prior_turn: u32, at_secs: u64 },
    /// The model changed between this turn and the previous one.
    ModelSwitch(Box<ModelSwitchBoundary>),
    /// A compaction occurred before this turn.
    Compacted(Box<CompactedBoundary>),
}

/// Facts for a model-switch boundary between turns.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModelSwitchBoundary {
    pub from: String,
    pub to: String,
    pub at_secs: u64,
}

/// Facts for a compaction boundary between turns.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CompactedBoundary {
    pub checkpoint_id: String,
    pub at_secs: u64,
}

#[derive(Clone)]
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

#[derive(Clone)]
pub struct TrajectoryTurn {
    pub n: usize,
    /// Boundaries the log recorded between the previous turn and this one, in
    /// the order they happened. Several durable facts can land in one gap (a
    /// compaction and then a model switch), so this is a list rather than a
    /// slot: a single slot would drop one of them without saying so. Data only;
    /// the pane decides how to draw them.
    pub boundary_before: Vec<TurnBoundary>,
    pub user_input: String,
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
    pub records: Vec<TrajectoryRecord>,
}

#[derive(Clone)]
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

/// The trajectory-data seam the render path calls. Matches the disk-search
/// pattern: the TUI owns the contract + the plain-data view; the
/// composition root injects an impl that holds the session id + reads the
/// durable session log and projects events into the view. None in stub and
/// unwired modes falls back to the mock so the pane still renders a demo.
pub trait TrajectoryLog: Send + Sync {
    /// Project the session's durable event log into the view. Called on every
    /// draw, so an implementation reuses its last projection while the log has
    /// not changed.
    fn trajectory(&self) -> TrajectoryView;

    /// Widen the loaded window by one page of older turns. Called when the user
    /// walks past the oldest loaded turn; an implementation with nothing older
    /// to load does nothing.
    fn load_older(&self) {}
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
/// The session totals already include it: the runner folds a child's usage into
/// the same cumulative tally the parent's calls go to. This type is the
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

#[derive(Clone)]
pub struct TrajectoryView {
    pub session_id: String,
    /// Derived: one model when every turn's model field matches (or is
    /// None); "N models" when ≥2 distinct ids appear. Replaces the
    /// construction-time string snapshot so a mid-session model switch
    /// surfaces immediately.
    pub model: String,
    pub total_turns: usize,
    pub tokens_in: Option<usize>,
    pub tokens_out: Option<usize>,
    pub cache_read: Option<u64>,
    pub failures: usize,
    pub duration_secs: u64,
    pub timing: SessionTiming,
    /// How many turns sit before the loaded window. Non-zero means older
    /// history exists and has not been read yet.
    pub hidden_turns: usize,
    /// What delegated sub-agents spent, when the session delegated any work.
    pub subagent_usage: Option<SubagentUsage>,
    pub rows: Vec<TrajectoryRow>,
}

#[path = "trajectory_detail.rs"]
mod detail;

#[path = "trajectory_list.rs"]
mod list;

#[path = "sample_trajectory.rs"]
mod sample;
use sample::sample_trajectory;

/// Main entry: dispatch on the drill level. Each level builder returns the
/// header and footer to pin plus a scrollable body and the body line the
/// selection sits on; header + footer stay pinned so the key hints never scroll
/// off. The selected body line, not the row index, drives the scroll offset:
/// a boundary separator occupies a body line without being a selectable row, so
/// the two are not the same number.
pub fn draw_content(f: &mut Frame, area: Rect, app: &crate::state::App) {
    // Real data when wired; fallback sample in unwired modes so the pane
    // still renders demonstration rows.
    let traj = app
        .trajectory_log
        .as_ref()
        .map(|l| l.trajectory())
        .unwrap_or_else(sample_trajectory);
    let level = app.trajectory_level.get();
    let cursor = app.trajectory_cursor.get();
    let turn_idx = app.trajectory_turn_idx.get();
    let (header, body, footer, sel_line) = match level {
        1 => detail::draw_turn_detail(&traj, turn_idx, cursor, area, app),
        2 => detail::draw_event_detail(&traj, turn_idx, cursor, area),
        _ => list::draw_turn_list(&traj, cursor, area),
    };
    // Stash the body length so the Up/Down handler can clamp the cursor in
    // [0, len-1] — without this Down past the last row drops the selection.
    let active_len = match level {
        1 => traj
            .rows
            .get(turn_idx)
            .map(|r| match r {
                TrajectoryRow::Turn(t) => t.records.len(),
                TrajectoryRow::Bg(_) => 0,
            })
            .unwrap_or(0),
        2 => 0,
        _ => traj.rows.len(),
    };
    if active_len > 0 && cursor >= active_len {
        app.trajectory_cursor.set(active_len.saturating_sub(1));
    }
    app.trajectory_list_len.set(active_len);
    render_scrolled(f, area, header, body, footer, sel_line);
}

/// Render a pane as a pinned header, a cursor-following scrollable body, and a
/// pinned footer. The body window is chosen so the cursor row stays in view
/// (centered when possible, clamped at the top/bottom of the body). This keeps
/// the key-hint footer visible no matter how many turns or events a level
/// holds — the half-pane height cannot clip the hints.
fn render_scrolled(
    f: &mut Frame,
    area: Rect,
    header: Vec<Line<'static>>,
    body: Vec<Line<'static>>,
    footer: Vec<Line<'static>>,
    sel_line: usize,
) {
    use ratatui::layout::{Constraint, Direction, Layout};
    let h = header.len() as u16;
    let ft = footer.len() as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(h),
            Constraint::Min(0),
            Constraint::Length(ft),
        ])
        .split(area);
    f.render_widget(Paragraph::new(header), chunks[0]);
    let visible = chunks[1].height as usize;
    let scroll = if body.len() <= visible {
        0
    } else {
        let half = visible / 2;
        sel_line
            .saturating_sub(half)
            .min(body.len().saturating_sub(visible))
    };
    f.render_widget(Paragraph::new(body).scroll((scroll as u16, 0)), chunks[1]);
    f.render_widget(Paragraph::new(footer), chunks[2]);
}

fn line(spans: Vec<Span<'static>>) -> Line<'static> {
    Line::from(spans)
}
fn blank() -> Line<'static> {
    Line::raw("")
}
fn sp(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(color))
}
fn fmt_k(n: usize) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{}", n)
    }
}

/// Format an optional token count: a real value via fmt_k, or "—" for None
/// (unknown — the turn had no TurnUsage, e.g. cancelled mid-stream). Never
/// renders 0 for an unknown count: a 0% display would read as "confirmed
/// zero" rather than "not measured".
fn fmt_k_opt(n: Option<usize>) -> String {
    match n {
        Some(v) => fmt_k(v),
        None => "—".to_string(),
    }
}
#[cfg(test)]
fn bar_width(ms: u64, total: u64, max_w: usize) -> usize {
    if total == 0 {
        0
    } else {
        ((ms as f64 / total as f64) * max_w as f64).round() as usize
    }
}

/// A fixed-width string (exactly width chars) with the event bar positioned
/// at its start offset on the shared turn time axis. Parallel events overlap
/// on the same columns of adjacent rows; sequence + duration + overlap are
/// visible at a glance. Unicode block elements, not ASCII hashes — the visual
/// standard for a Gantt-style trace (Jaeger/Zipkin render bars this way).
fn positioned_bar(start_ms: u64, dur_ms: u64, total_ms: u64, width: usize) -> String {
    if width == 0 || total_ms == 0 {
        return " ".repeat(width);
    }
    let scale = width as f64 / total_ms as f64;
    let start_col = ((start_ms as f64 * scale) as usize).min(width - 1);
    if dur_ms == 0 {
        // Instant event (a gate deny fires at one moment): a thin marker.
        let mut s = " ".repeat(start_col);
        s.push('┃');
        while s.chars().count() < width {
            s.push(' ');
        }
        return s;
    }
    let end_col = (((start_ms + dur_ms) as f64 * scale) as usize)
        .max(start_col + 1)
        .min(width);
    let n = end_col - start_col;
    let mut s = " ".repeat(start_col);
    s.push_str(&"█".repeat(n));
    while s.chars().count() < width {
        s.push(' ');
    }
    s
}

/// One ruler line above the event rows, oriented to the bar area: "0s" left,
/// the turn total right, a dotted axis between. Orients the eye to the time
/// scale so positioned bars read as a real timeline.
fn ruler_line(total_ms: u64, width: usize) -> Line<'static> {
    let pre = 10; // align with the bar start (prefix 2 + kind 7 + gap 1)
    let left = "0s".to_string();
    let right = format!("{:.1}s", total_ms as f64 / 1000.0);
    let mut s = " ".repeat(pre);
    s.push_str(&left);
    let fill = width.saturating_sub(s.chars().count() + right.chars().count());
    s.push_str(&"·".repeat(fill));
    s.push_str(&right);
    line(vec![sp(s, Color::DarkGray)])
}

#[cfg(test)]
#[path = "trajectory_pane_tests.rs"]
mod tests;
