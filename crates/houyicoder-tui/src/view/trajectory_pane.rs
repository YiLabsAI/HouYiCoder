//! The /trajectory pane: a turn-organized distributed-trace view with a
//! 3-level drill-down and an ASCII time axis. Follows the /memory
//! /permissions pane shape (shared draw_command_pane template) but
//! renders a session timeline. Mock data for now — real wiring arrives
//! when the observability log is connected to the agent loop.
//!
//! 3 levels:
//! - Level 0: session summary + turn list (collapsed, cursor selects)
//! - Level 1: turn detail — event list + ASCII bar (cursor selects)
//! - Level 2: event detail — full data (long content folded)
//!
//! Keys: Up/Down move cursor at current level, Enter drills down,
//! Esc goes back one level (or closes at level 0).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::view::line_wrap::truncate_width;

// Data types

#[derive(Clone)]
pub struct TrajectoryEvent {
    pub kind: String,
    pub summary: String,
    /// Offset from the turn start, in ms. Positions the event on the shared
    /// time axis so parallel events overlap on the same columns and sequence
    /// is visible at a glance — not just duration.
    pub start_ms: u64,
    pub duration_ms: u64,
    pub success: bool,
    /// Full text for the Level 2 detail view. kind-specific:
    /// llm = the reasoning/thinking text; bash/read/edit = the command or
    /// target input; output = the tool result or model output. Held separate
    /// from summary (which is the one-line preview for L1) so L2 can show the
    /// full content without re-truncating.
    pub thinking: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
}

#[derive(Clone)]
pub struct TrajectoryTurn {
    pub n: usize,
    pub user_input: String,
    pub tokens_in: Option<usize>,
    pub tokens_out: Option<usize>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    /// Per-turn model id (the resolved id, not the tier label). None when
    /// no TurnUsage landed or the log predates the field — unknown, not
    /// blank (I13).
    pub model: Option<String>,
    /// Per-turn effort level actually sent. None when no effort parameter
    /// was sent (model unsupported, auto, or old log) — unknown, not auto.
    pub effort: Option<String>,
    /// Per-turn reasoning tokens (a component of output_tokens, not a
    /// separate total). None when no TurnUsage or the log predates the
    /// field — unknown, not zero (I13).
    pub reasoning_tokens: Option<usize>,
    pub tool_count: usize,
    pub tool_fail: usize,
    pub retries: usize,
    pub duration_ms: u64,
    pub success: bool,
    pub events: Vec<TrajectoryEvent>,
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
    /// Project the bridge's session's durable event log into the view.
    fn trajectory(&self) -> TrajectoryView;
}

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
    pub failures: usize,
    pub duration_secs: u64,
    pub rows: Vec<TrajectoryRow>,
}

#[path = "mock_trajectory.rs"]
mod mock;
use mock::mock_trajectory;

/// The display title for a turn: the user input when present, otherwise a
/// derived fallback so a tool-continuation turn (no user input — the model
/// auto-continued after a tool result) does not show a blank title. The
/// fallback is the first event's summary (the first tool call or LLM step),
/// prefixed with "(continued)" to distinguish it from a real prompt. When
/// the turn has no events either, "(no input)" surfaces.
fn turn_title(turn: &TrajectoryTurn) -> String {
    if !turn.user_input.trim().is_empty() {
        return turn.user_input.clone();
    }
    if let Some(first) = turn.events.first() {
        if first.summary.trim().is_empty() {
            return "(continued)".to_string();
        }
        format!("(continued) {}", first.summary)
    } else {
        "(no input)".to_string()
    }
}

// Rendering

/// Main entry: dispatch on the drill level. Each level builder returns
/// (header, body, footer) line groups; the body is the scrollable cursor list,
/// header + footer stay pinned so the key hints never scroll off. The body
/// scroll offset tracks the cursor so the selected row is always visible.
pub fn draw_content(f: &mut Frame, area: Rect, app: &crate::state::App) {
    // Real data when the composition root wired a bridge (reads the durable
    // log and projects); mock fallback in stub and unwired modes so the pane
    // still renders a demo.
    let traj = app
        .trajectory_log
        .as_ref()
        .map(|l| l.trajectory())
        .unwrap_or_else(mock_trajectory);
    let level = app.trajectory_level.get();
    let cursor = app.trajectory_cursor.get();
    let turn_idx = app.trajectory_turn_idx.get();
    let (header, body, footer) = match level {
        1 => draw_turn_detail(&traj, turn_idx, cursor, area, app),
        2 => draw_event_detail(&traj, turn_idx, cursor, area),
        _ => draw_turn_list(&traj, cursor, area),
    };
    // Stash the body length so the Up/Down handler can clamp the cursor in
    // [0, len-1] — without this Down past the last row drops the selection.
    app.trajectory_list_len.set(body.len());
    render_scrolled(f, area, header, body, footer, cursor);
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
    cursor: usize,
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
        cursor
            .saturating_sub(half)
            .min(body.len().saturating_sub(visible))
    };
    f.render_widget(Paragraph::new(body).scroll((scroll as u16, 0)), chunks[1]);
    f.render_widget(Paragraph::new(footer), chunks[2]);
}

/// Level 0: session summary header + turn list body. The cursor selects a row
/// (a turn or a background event); Enter drills into the focused turn.
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
fn draw_turn_list(
    traj: &TrajectoryView,
    cursor: usize,
    _area: Rect,
) -> (Vec<Line<'static>>, Vec<Line<'static>>, Vec<Line<'static>>) {
    let total_calls: usize = traj
        .rows
        .iter()
        .map(|r| match r {
            TrajectoryRow::Turn(t) => t.tool_count,
            _ => 0,
        })
        .sum();
    let header = vec![
        line(vec![
            sp(format!("{} turns", traj.total_turns), Color::Cyan),
            sp(" · ", Color::DarkGray),
            sp(
                format!(
                    "{}↓ {}↑",
                    fmt_k_opt(traj.tokens_in),
                    fmt_k_opt(traj.tokens_out)
                ),
                Color::Gray,
            ),
            sp(" · ctx 42%", Color::DarkGray),
            sp(format!(" · {} calls", total_calls), Color::Gray),
            sp(format!(" · {} fail", traj.failures), Color::Red),
            sp(format!(" · {}s", traj.duration_secs), Color::Gray),
        ]),
        line(vec![
            sp(traj.model.clone(), Color::DarkGray),
            sp(" · auto-memory on · auto-dream on", Color::DarkGray),
        ]),
        blank(),
    ];
    // Per-turn model/effort attribution: render only when the session saw
    // ≥2 distinct model ids (otherwise every row would repeat the same id
    // — noise, not signal). When ≥2, each turn that has a model shows it.
    let show_per_turn_model = traj
        .rows
        .iter()
        .filter_map(|r| match r {
            TrajectoryRow::Turn(t) => t.model.as_deref(),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>()
        .len()
        >= 2;
    let mut body = Vec::new();
    let clamped = cursor.min(traj.rows.len().saturating_sub(1));
    for (i, row) in traj.rows.iter().enumerate() {
        let sel = i == clamped;
        let prefix = if sel { "▸ " } else { "  " };
        match row {
            TrajectoryRow::Turn(t) => {
                let glyph = if t.success { "✓" } else { "✗" };
                let gc = if t.success { Color::Green } else { Color::Red };
                body.push(line(vec![
                    sp(prefix, Color::Cyan),
                    sp(format!("T{} ", t.n), Color::Cyan),
                    sp(
                        format!("{:32} ", truncate_width(&turn_title(t), 32)),
                        Color::White,
                    ),
                    sp(
                        format!("{}↓ {}↑", fmt_k_opt(t.tokens_in), fmt_k_opt(t.tokens_out),),
                        Color::Gray,
                    ),
                    // Thinking tokens: shown as (thinking Nk) only when
                    // reasoning_tokens is Some and >0. The parenthetical
                    // is tight against the output number with no separator
                    // so the inclusion relation (reasoning ⊂ output) is
                    // visually unambiguous (I14).
                    match t.reasoning_tokens {
                        Some(r) if r > 0 => {
                            sp(format!(" (thinking {})", fmt_k(r)), Color::DarkGray)
                        }
                        _ => sp(String::new(), Color::DarkGray),
                    },
                    // Per-turn model/effort: only when ≥2 distinct models
                    // in the session (noise otherwise). Old logs (None) are
                    // omitted, not filled with the current value (I13).
                    if show_per_turn_model {
                        match &t.model {
                            Some(m) => sp(format!("  {m}"), Color::DarkGray),
                            None => sp(String::new(), Color::DarkGray),
                        }
                    } else {
                        sp(String::new(), Color::DarkGray)
                    },
                    // Per-turn effort: only alongside per-turn model (effort
                    // without model context is meaningless). None = omitted.
                    if show_per_turn_model {
                        match &t.effort {
                            Some(e) => sp(format!(" {e}"), Color::DarkGray),
                            None => sp(String::new(), Color::DarkGray),
                        }
                    } else {
                        sp(String::new(), Color::DarkGray)
                    },
                    sp(format!("  {} calls ", t.tool_count), Color::Gray),
                    sp(
                        format!("{} fail  ", t.tool_fail),
                        if t.tool_fail > 0 {
                            Color::Red
                        } else {
                            Color::DarkGray
                        },
                    ),
                    sp(
                        format!("{:.1}s ", t.duration_ms as f64 / 1000.0),
                        Color::Gray,
                    ),
                    sp(glyph, gc),
                ]));
            }
            TrajectoryRow::Bg(bg) => {
                body.push(line(vec![
                    sp(prefix, Color::Cyan),
                    sp("[bg] ", Color::DarkGray),
                    sp(format!("{:8} ", bg.kind), Color::DarkGray),
                    sp(truncate_width(&bg.summary, 50), Color::DarkGray),
                    sp(
                        format!("  {:.1}s", bg.duration_ms as f64 / 1000.0),
                        Color::DarkGray,
                    ),
                ]));
            }
        }
    }
    let footer = vec![
        blank(),
        line(vec![sp(
            " Up/Down select - Enter expand - Esc close",
            Color::DarkGray,
        )]),
    ];
    (header, body, footer)
}

/// Level 1: turn title header + a positional Gantt timeline of events. Each
/// event row's bar is positioned at its start offset on the shared turn time
/// axis (width = duration), so parallel events overlap on the same columns and
/// the latency hot-spots are visible at a glance. A ruler line orients the
/// scale. This is the visual standard for a trace timeline (Jaeger/Zipkin),
/// adapted to the TUI with unicode block elements.
fn draw_turn_detail(
    traj: &TrajectoryView,
    turn_idx: usize,
    cursor: usize,
    area: Rect,
    app: &crate::state::App,
) -> (Vec<Line<'static>>, Vec<Line<'static>>, Vec<Line<'static>>) {
    let mut header = Vec::new();
    let mut body = Vec::new();
    let Some(row) = traj.rows.get(turn_idx) else {
        return (
            vec![line(vec![sp("no row data", Color::DarkGray)])],
            vec![],
            vec![],
        );
    };
    let row = row.clone();
    match row {
        TrajectoryRow::Turn(turn) => {
            app.trajectory_at_bg.set(false);
            let clamped = cursor.min(turn.events.len().saturating_sub(1));
            header.push(line(vec![
                sp(
                    format!(
                        " T{}  \"{}\"",
                        turn.n,
                        truncate_width(&turn_title(&turn), 30)
                    ),
                    Color::Cyan,
                ),
                sp(
                    format!(
                        "  {}↓ {}↑ c{} · total {:.1}s",
                        fmt_k_opt(turn.tokens_in),
                        fmt_k_opt(turn.tokens_out),
                        fmt_k_opt(turn.cache_read.map(|v| v as usize)),
                        turn.duration_ms as f64 / 1000.0
                    ),
                    Color::Gray,
                ),
            ]));
            header.push(blank());
            // Layout: prefix(2) + kind(7) + gap(1) + bar(bar_area) + gap(1) + dur(7) + gap(1) + summary.
            // 50 covers everything but the bar; the bar takes the remainder.
            let bar_area = (area.width as usize).saturating_sub(50).max(8);
            let summary_w = (area.width as usize).saturating_sub(20 + bar_area).max(8);
            header.push(ruler_line(turn.duration_ms, bar_area));
            for (i, ev) in turn.events.iter().enumerate() {
                let sel = i == clamped;
                let prefix = if sel { "▸ " } else { "  " };
                let bar = positioned_bar(ev.start_ms, ev.duration_ms, turn.duration_ms, bar_area);
                let bc = if ev.success { Color::Green } else { Color::Red };
                let mark = if ev.success { "✓" } else { "✗" };
                body.push(line(vec![
                    sp(prefix, Color::Cyan),
                    sp(format!("{:7}", ev.kind), Color::DarkGray),
                    sp(" ", Color::DarkGray),
                    sp(bar, bc),
                    sp(" ", Color::DarkGray),
                    sp(
                        format!("{:>5.1}s", ev.duration_ms as f64 / 1000.0),
                        Color::Gray,
                    ),
                    sp(" ", Color::DarkGray),
                    sp(truncate_width(&ev.summary, summary_w), Color::White),
                    sp(format!(" {}", mark), bc),
                ]));
            }
            let footer = vec![
                blank(),
                line(vec![sp(
                    " Up/Down select · Enter detail · Esc back",
                    Color::DarkGray,
                )]),
            ];
            (header, body, footer)
        }
        TrajectoryRow::Bg(bg) => {
            // A [bg] row drilled from L0 has no event timeline — show its
            // detail directly at L1 and flag it so Enter does not drill to L2.
            app.trajectory_at_bg.set(true);
            header.push(line(vec![
                sp(format!(" [bg] {} ", bg.kind), Color::Cyan),
                sp(truncate_width(&bg.summary, 50), Color::White),
                sp(
                    format!("  {:.1}s", bg.duration_ms as f64 / 1000.0),
                    Color::Gray,
                ),
            ]));
            header.push(blank());
            body.push(line(vec![
                sp(" kind: ", Color::DarkGray),
                sp(bg.kind.clone(), Color::Gray),
            ]));
            body.push(line(vec![
                sp(" summary: ", Color::DarkGray),
                sp(bg.summary.clone(), Color::White),
            ]));
            body.push(blank());
            body.push(line(vec![
                sp(" latency: ", Color::DarkGray),
                sp(format!("{}ms", bg.duration_ms), Color::Gray),
            ]));
            let footer = vec![blank(), line(vec![sp(" Esc back", Color::DarkGray)])];
            (header, body, footer)
        }
    }
}

/// Level 2: the full detail of the event selected at Level 1 (the cursor is
/// the event index, frozen on drill — Up/Down is disabled at this level so
/// the view is stable, not a switcher). Shows the full thinking text, tool
/// input, and tool output (multi-line) rather than the one-line L1 summary —
/// this is the deepest level where the actual content lives.
fn draw_event_detail(
    traj: &TrajectoryView,
    turn_idx: usize,
    cursor: usize,
    _area: Rect,
) -> (Vec<Line<'static>>, Vec<Line<'static>>, Vec<Line<'static>>) {
    let mut header = Vec::new();
    let mut body = Vec::new();
    let turn = traj.rows.get(turn_idx).and_then(|r| match r {
        TrajectoryRow::Turn(t) => Some(t),
        _ => None,
    });
    let Some(turn) = turn else {
        return (header, body, vec![]);
    };
    let ev = turn.events.get(cursor).or_else(|| turn.events.first());
    let Some(ev) = ev else {
        return (header, body, vec![]);
    };
    let idx = cursor.min(turn.events.len().saturating_sub(1));
    let mark = if ev.success { "✓" } else { "✗" };
    let mc = if ev.success { Color::Green } else { Color::Red };
    header.push(line(vec![
        sp(format!(" {} ", ev.kind), Color::Cyan),
        sp(truncate_width(&ev.summary, 48), Color::White),
        sp(
            format!("  {:.1}s ", ev.duration_ms as f64 / 1000.0),
            Color::Gray,
        ),
        sp(mark, mc),
        sp(
            format!("  · event {}/{}", idx + 1, turn.events.len()),
            Color::DarkGray,
        ),
    ]));
    header.push(blank());

    // Push a labeled field; multi-line strings split into one line per row so
    // the full content shows instead of being squashed onto one line.
    let push_field = |body: &mut Vec<Line<'static>>, label: &str, text: &str, color: Color| {
        let mut first = true;
        for line_text in text.split('\n') {
            let prefix = if first {
                format!(" {label}: ",)
            } else {
                "        ".to_string()
            };
            body.push(line(vec![
                sp(prefix, Color::DarkGray),
                sp(line_text.to_string(), color),
            ]));
            first = false;
        }
    };

    // L2 detail renders every field the event CARRIES, by presence — not a
    // kind-name table. A kind-name table would mask the gap where the
    // projection + the mock emit different kind strings (real "tool_call" /
    // "tool_result" / "reasoning" vs mock "bash" / "read" / "edit"), so a
    // real session drilled to L2 showed an empty body while the mock
    // rendered fine. Field-presence drives rendering for every kind,
    // present + future, and the header already labels the kind name.
    // Redact secrets before rendering — the trajectory pane is a human-facing
    // surface (screen-share / recording / scrollback), and tool I/O + reasoning
    // can carry real secrets (an .env cat, a credentials read). The durable
    // log stays full-fidelity; only the display is filtered. See redaction.rs.
    if let Some(thinking) = &ev.thinking {
        let r = crate::redaction::redact(thinking);
        push_field(&mut body, "thinking", &r, Color::Gray);
    }
    if let Some(input) = &ev.input {
        let r = crate::redaction::redact(input);
        push_field(&mut body, "input", &r, Color::Gray);
    }
    if let Some(output) = &ev.output {
        let r = crate::redaction::redact(output);
        push_field(&mut body, "output", &r, mc);
    }
    body.push(blank());
    body.push(line(vec![
        sp(" latency: ", Color::DarkGray),
        sp(format!("{}ms", ev.duration_ms), Color::Gray),
        sp("  · start: ", Color::DarkGray),
        sp(format!("{}ms", ev.start_ms), Color::Gray),
    ]));
    let footer = vec![line(vec![sp(" Esc back", Color::DarkGray)])];
    (header, body, footer)
}

// Helpers

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
