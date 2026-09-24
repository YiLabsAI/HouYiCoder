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

use crate::state::{TrajectoryDrill, TrajectoryPaneState, TrajectoryTurnKey};
use crate::view::line_wrap::truncate_width;
use crate::view::navigation::key_hint;
use crate::view::relative_time::{format_span_ms, now_epoch_secs, relative_time};
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

mod detail;
mod list;
mod sample;
mod view;

use sample::sample_trajectory;

pub use detail::{TrajectoryDetailState, TrajectoryDetailView};
pub use view::{
    SessionTiming, SubagentUsage, TrajectoryBg, TrajectoryRecord, TrajectoryRow, TrajectoryTurn,
    TrajectoryView, TrajectoryViewState,
};

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
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
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
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
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

/// Facts for a compaction boundary between turns. The token counts bracket
/// the fold, so the separator shows what it reclaimed. Both are zero on a log
/// written before the counts were recorded.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CompactedBoundary {
    pub checkpoint_id: String,
    pub pre_tokens: u64,
    pub post_tokens: u64,
    pub at_secs: u64,
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
    fn trajectory(&self) -> std::sync::Arc<TrajectoryView>;

    /// Widen the loaded window by one page of older turns. Called when the user
    /// walks past the oldest loaded turn; an implementation with nothing older
    /// to load does nothing.
    fn load_older(&self) {}

    /// Replace the window with the session's oldest turns. Called when the user
    /// asks for the head; an implementation with nothing to page does nothing.
    fn load_earliest(&self) {}

    /// Replace the window with the newest turns. Called when the user asks to
    /// return to the tail; an implementation that already shows it does nothing.
    fn return_to_tail(&self) {}

    /// Ask for one turn's records, by the key its row carries. Called when the
    /// user opens a turn; an implementation reads them off the draw path and
    /// answers from its cache after that.
    fn request_detail(&self, drill: &TrajectoryDrill);

    /// What is known about a turn's records. A draw calls this, so it must not
    /// read the log: it serves what the read has produced so far.
    fn detail(&self, key: &TrajectoryTurnKey) -> std::sync::Arc<TrajectoryDetailView>;
}

/// Put the cursor on the turn the selection names, when the window in hand
/// holds that turn.
///
/// A row index alone would name a different turn once a page arrived under it,
/// which is why the selection is a turn number. A selection made in another
/// history is dropped instead of restored: a clear starts a history whose turn
/// numbers begin again, so the number would name a turn it does not mean.
pub(crate) fn restore_selected_cursor(state: &TrajectoryPaneState, view: &TrajectoryView) {
    let Some(selected) = state.selection() else {
        return;
    };
    if selected.history_generation != view.history_generation {
        state.clear_selection();
        return;
    }
    if let Some(index) = view
        .rows
        .iter()
        .position(|row| matches!(row, TrajectoryRow::Turn(turn) if turn.n == selected.number))
    {
        state.set_cursor(index);
    }
}

/// Record the turn the L0 cursor sits on, so a page that arrives under it can
/// put the cursor back on the same turn instead of the same row index.
///
/// A background row is not a turn, so nothing is selected there: keeping the
/// turn the user moved off would let a later page restore the cursor onto it.
pub(crate) fn note_selected_turn(state: &TrajectoryPaneState, view: &TrajectoryView) {
    match view.rows.get(state.cursor()) {
        Some(TrajectoryRow::Turn(turn)) => state.select(turn.n, view.history_generation),
        _ => state.clear_selection(),
    }
}

/// The row the drill is about, when the window in hand still holds it.
///
/// A row index names a different turn once a page arrives under it, and the
/// window can move while the user reads one turn, so the drill follows the
/// turn it named. None means the turn is not in the window: the window moved
/// past it, or the history it belonged to was cleared. The frozen index is
/// not a fallback there, because it names another turn by then.
fn drilled_row(state: &TrajectoryPaneState, view: &TrajectoryView) -> Option<usize> {
    let Some(drill) = state.drill() else {
        // A background row has no turn to follow, so it keeps the row the
        // drill froze and its own level-1 only contract.
        return Some(state.turn_idx());
    };
    if drill.history_generation != view.history_generation {
        return None;
    }
    let index = view
        .rows
        .iter()
        .position(|row| matches!(row, TrajectoryRow::Turn(turn) if turn.key == drill.key))?;
    state.set_turn_idx(index);
    Some(index)
}

/// Record the turn the drill levels are about, by the key its row carries.
///
/// The key is the identity the detail read is asked for by, so the drill holds
/// it rather than the row it happened to sit on.
pub(crate) fn note_drilled_row(state: &TrajectoryPaneState, view: &TrajectoryView) {
    match view.rows.get(state.cursor()) {
        Some(TrajectoryRow::Turn(turn)) => state.set_drill(TrajectoryDrill {
            key: turn.key.clone(),
            number: turn.n,
            history_generation: view.history_generation,
        }),
        _ => state.clear_drill(),
    }
}

/// The records of the turn the drill is on, asked for once and then served from
/// the source's cache.
fn detail_for(
    app: &crate::state::App,
    drill: Option<&TrajectoryDrill>,
) -> Arc<TrajectoryDetailView> {
    let Some(drill) = drill else {
        return Arc::new(TrajectoryDetailView::default());
    };
    match app.trajectory_log.as_ref() {
        Some(log) => {
            // Asking again is how a draw polls; the source answers from what it
            // has and asks the disk once per drill.
            log.request_detail(drill);
            log.detail(&drill.key)
        }
        // Unwired: the demonstration answers, the same way it serves the rows.
        None => Arc::new(sample::sample_detail(&drill.key)),
    }
}

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
        .unwrap_or_else(|| std::sync::Arc::new(sample_trajectory()));
    let level = app.trajectory.level();
    // The window slides, so the selection is a turn number and the row is
    // found from it again rather than carried as an index.
    if level == 0 {
        restore_selected_cursor(&app.trajectory, &traj);
    }
    let cursor = app.trajectory.cursor();
    // The drill levels render the turn the drill named, not the row index it
    // had when the drill started: a page can arrive under that index while the
    // user reads.
    let drilled = match level {
        0 => None,
        _ => drilled_row(&app.trajectory, &traj),
    };
    // The records come from the detail seam, asked for by the key the drill
    // holds: a draw serves what the read has produced.
    let row = drilled.map(|index| &traj.rows[index]);
    // The drill names the turn; a row in hand names it too, which is what the
    // level renders from before the drill is recorded.
    let drill = app.trajectory.drill().or_else(|| match row {
        Some(TrajectoryRow::Turn(turn)) => Some(TrajectoryDrill {
            key: turn.key.clone(),
            number: turn.n,
            history_generation: traj.history_generation,
        }),
        _ => None,
    });
    let detail = detail_for(app, drill.as_ref());
    // The turn's own facts come from the detail once it is in hand: the row it
    // came from may have left the window by then.
    let turn = detail.turn.clone().or_else(|| match row {
        Some(TrajectoryRow::Turn(turn)) => Some(turn.clone()),
        _ => None,
    });
    let (header, body, footer, sel_line) = match level {
        1 => match (turn.as_ref(), row) {
            (Some(turn), _) => detail::draw_turn_detail(turn, &detail, cursor, area, app),
            (None, Some(TrajectoryRow::Bg(bg))) => detail::draw_bg_detail(bg, cursor, area, app),
            _ => detail::draw_drill_gone(),
        },
        2 => match turn.as_ref() {
            Some(turn) => detail::draw_event_detail(turn, &detail, cursor, area),
            None => detail::draw_drill_gone(),
        },
        _ => list::draw_turn_list(&traj, cursor, area),
    };
    // Stash the body length so the Up/Down handler can clamp the cursor in
    // [0, len-1] — without this Down past the last row drops the selection.
    let active_len = match level {
        1 => detail.records.len(),
        0 => traj.rows.len(),
        _ => 0,
    };
    if active_len > 0 && cursor >= active_len {
        app.trajectory.set_cursor(active_len.saturating_sub(1));
    }
    app.trajectory.set_list_len(active_len);
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

/// Pad a string to a display width, counting columns rather than characters
/// so a wide glyph cannot shift the columns after it.
fn pad(text: &str, width: usize) -> String {
    let w = UnicodeWidthStr::width(text);
    if w >= width {
        return text.to_string();
    }
    let mut out = text.to_string();
    out.push_str(&" ".repeat(width - w));
    out
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

/// The columns a level 1 timeline row spends before its bar: the selection
/// prefix, the kind, a gap, and the name. The ruler above the rows starts its
/// axis at this column, so the two are read as one scale.
const TIMELINE_PREFIX_W: usize = 2 + 7 + 1 + TIMELINE_NAME_W;

/// The name column inside the prefix, in display columns.
const TIMELINE_NAME_W: usize = 11;

/// The gap between two columns.
const TIMELINE_GAP_W: usize = 1;

/// The duration column, including the space after it.
const TIMELINE_DUR_W: usize = 7;

/// The summary column's floor, so a narrow terminal shrinks the bar first.
const TIMELINE_SUMMARY_MIN_W: usize = 32;

/// The outcome glyph a row ends with, and the space before it.
const TIMELINE_MARK_W: usize = 2;

/// The columns a row spends after its bar: a gap, the duration, a gap, and the
/// mark.
const TIMELINE_SUFFIX_W: usize = TIMELINE_GAP_W + TIMELINE_DUR_W + TIMELINE_GAP_W + TIMELINE_MARK_W;

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
    let left = "0s".to_string();
    let right = format_span_ms(total_ms);
    // The axis spans exactly the bar's columns, so a bar's position on the
    // ruler is its position in the turn.
    let mut s = " ".repeat(TIMELINE_PREFIX_W);
    s.push_str(&left);
    let fill = width.saturating_sub(
        UnicodeWidthStr::width(left.as_str()) + UnicodeWidthStr::width(right.as_str()),
    );
    s.push_str(&"·".repeat(fill));
    s.push_str(&right);
    line(vec![sp(s, Color::DarkGray)])
}

#[cfg(test)]
mod tests;
