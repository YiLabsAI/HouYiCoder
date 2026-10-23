//! Footer fleet strip: one row per spawned child, shown only while the
//! fleet is non-empty. Each row carries the child's hollow-circle glyph,
//! its type, a verb inferred from the last tool, and cumulative tokens.
//! Shift-arrow moves the strip's selection; the selected row gets a prefix
//! and Enter drills into its teammate view. The strip caps at three rows
//! and scrolls toward the selection when the fleet is longer.

use std::time::Duration;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::agent_message::FleetEntry;
use crate::command::render::format_tokens;
use crate::state::App;

/// Max child rows the strip ever shows. A longer fleet scrolls within the
/// strip so the transcript is never pushed off-screen.
const MAX_VISIBLE: usize = 3;

/// Rows the strip would like: one per child, capped so a large fleet scrolls
/// within the strip instead of pushing the transcript off-screen. Zero when
/// the fleet is empty. The shared footer budget decides what it gets.
pub fn want(app: &App) -> u16 {
    let entries = &app.sessions.current.capabilities.fleet.entries;
    entries.len().min(MAX_VISIBLE).min(u16::MAX as usize) as u16
}

/// Draw the strip into the granted area. One row for a multi-child fleet is
/// the collapsed form: a single row of per-child detail would name one child
/// and silently omit the rest, so it becomes a count instead.
pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let entries = &app.sessions.current.capabilities.fleet.entries;
    let lines = if area.height == 1 && entries.len() > 1 {
        vec![summary_line(app)]
    } else {
        build_lines(app, area.height as usize)
    };
    f.render_widget(Paragraph::new(lines), area);
}

/// The collapsed form: how many children are live and how many just landed.
fn summary_line(app: &App) -> Line<'static> {
    let entries = &app.sessions.current.capabilities.fleet.entries;
    let done = entries.iter().filter(|e| e.completed.is_some()).count();
    let running = entries.len() - done;
    let mut text = format!("  {running} running");
    if done > 0 {
        text.push_str(&format!(" \u{00b7} {done} done"));
    }
    Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))
}

/// What a click in the strip does. The summary row of a multi-child fleet
/// names no child, so it opens the pane — the surface its own hint names.
/// A child row selects; the already-selected row drills in.
#[derive(Debug)]
pub enum FleetClick {
    OpenAgentsPane,
    Select(usize),
    Drill(String),
}

/// Pure routing of a click at the strip's given row. Kept free of App so
/// the decision is testable without one; the mouse handler applies it. The
/// entries are the session's fleet facts and the selection is the strip's
/// interaction state, so the caller passes both.
pub fn click_route(
    entries: &[FleetEntry],
    selected: Option<usize>,
    granted: u16,
    row: usize,
) -> FleetClick {
    let n = entries.len();
    let g = granted as usize;
    if g == 1 && n > 1 {
        return FleetClick::OpenAgentsPane;
    }
    let visible = visible_rows(n, g);
    let idx = window_start_idx(selected, n, visible) + row;
    let Some(entry) = entries.get(idx) else {
        return FleetClick::OpenAgentsPane;
    };
    if selected == Some(idx) {
        FleetClick::Drill(entry.agent_id.clone())
    } else {
        FleetClick::Select(idx)
    }
}

/// The sliding window's start as a free function, so click_route and the
/// draw share one definition without either needing an App.
fn window_start_idx(selected: Option<usize>, len: usize, visible: usize) -> usize {
    selected
        .map(|s| s.min(len.saturating_sub(visible)))
        .unwrap_or(0)
}

/// Rows of the sliding window: the granted height capped at the fleet size.
/// The strip spends every row it gets on a child rather than on a "+N more"
/// line. Such a line is misleading in a sliding window — the hidden count is
/// constant, but once the user scrolls the hidden rows sit above the window,
/// so "+N more" reads as "below" when the tail is already in view. The user
/// discovers more rows by scrolling; completed entries drop in the grace
/// window, so a hidden completed row is ephemeral.
fn visible_rows(len: usize, granted: usize) -> usize {
    granted.min(len)
}

/// Build the sliding window of strip rows, capped by the granted height.
/// The window follows the selection so the highlighted row stays on screen;
/// the strip spends every row it gets on a child rather than on an
/// overflow indicator (see visible_rows for why that line is dropped).
fn build_lines(app: &App, granted: usize) -> Vec<Line<'_>> {
    let entries = &app.sessions.current.capabilities.fleet.entries;
    let selected = app.ui.fleet.selected;
    let len = entries.len();
    if len == 0 || granted == 0 {
        return Vec::new();
    }
    let visible = visible_rows(len, granted);
    let start = window_start_idx(selected, len, visible);
    let end = (start + visible).min(len);
    // The child whose ask is on screen. Its row says so instead of showing a
    // verb: the child is not working, it is stopped waiting for the user, and
    // a strip that keeps saying "building" hides why nothing is moving.
    let awaiting = app
        .sessions
        .current
        .prompt
        .as_ref()
        .and_then(|p| p.approval())
        .and_then(|card| card.delegation.as_ref())
        .map(|d| d.child_id.as_str());
    entries[start..end]
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let abs_idx = start + i;
            let selected = abs_idx == selected.unwrap_or(usize::MAX);
            build_row(entry, selected, awaiting)
        })
        .collect()
}

/// One strip row. Completed children dim and go terse — the live verb drops,
/// leaving the type + a done marker + the token total — so a finished
/// delegation reads "explore · done · 1.2k tok" instead of echoing the stale
/// last verb. The running child keeps the verb and turn counter so the user
/// sees live progress at a glance.
fn build_row<'a>(entry: &'a FleetEntry, selected: bool, awaiting: Option<&str>) -> Line<'a> {
    let prefix = if selected { "> " } else { "  " };
    let (glyph, style) = if entry.completed.is_some() {
        ("✓ ", Style::default().fg(Color::DarkGray))
    } else {
        ("◯ ", Style::default().fg(Color::Cyan))
    };
    let tokens = format!("{} tok", format_tokens(entry.tokens));
    let body = if entry.completed.is_some() {
        format!("{} · done · {}", entry.subagent_type, tokens)
    } else if awaiting == Some(entry.agent_id.as_str()) {
        let elapsed = entry
            .started_at
            .map(|t| format!(" · {}", format_elapsed(t.elapsed())))
            .unwrap_or_default();
        format!(
            "{}: waiting for approval · {}{}",
            entry.subagent_type, tokens, elapsed
        )
    } else {
        let elapsed = entry
            .started_at
            .map(|t| format!(" · {}", format_elapsed(t.elapsed())))
            .unwrap_or_default();
        format!(
            "{}: {} · {} · turn {}{}",
            entry.subagent_type,
            verb_for(entry.last_activity.as_deref()),
            tokens,
            entry.turn,
            elapsed
        )
    };
    Line::from(vec![
        Span::styled(prefix.to_string(), style),
        Span::styled(glyph.to_string(), style),
        Span::styled(body, style),
    ])
}

/// Infer a one-word status verb from the child's last tool name. The bus
/// carries the tool name verbatim; the strip humanizes it so the row reads
/// "explore: searching" rather than "explore: grep".
fn verb_for(tool: Option<&str>) -> &'static str {
    match tool {
        Some("read") | Some("glob") => "reading",
        Some("grep") | Some("search") => "searching",
        Some("edit") | Some("write") => "writing",
        Some("bash") => "building",
        Some("test") => "verifying",
        Some(_) => "working",
        None => "thinking",
    }
}

/// Compact elapsed: under 60s as "Ns", under 1h as "Nm Ms", otherwise
/// "Nh Nm Ns" so a long-running child does not show "312s" and retains
/// the finer unit at each scale.
fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        format!("{}h {}m {}s", h, m, s)
    }
}

#[cfg(test)]
mod tests;
