//! Shared pane navigation presentation.
//!
//! Tab headers and key-action footers use one visual grammar so sibling panes
//! do not drift in spacing, active markers, action verbs, or hierarchy hints.

use std::cell::Cell;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::view::line_wrap::max_scroll_offset;

/// Build a tab row with one bracketed active label.
pub(crate) fn tab_header<T: Copy + Eq>(active: T, tabs: &[(T, &str)]) -> Line<'static> {
    tab_header_at(active, tabs, "  ")
}

/// Build a tab row whose first label starts at the given left margin.
///
/// The margin exists because a tab bar has to line up with the content under
/// it: a pane whose body is indented two columns wants the same indent here,
/// and a pane whose body starts at the left edge wants none. Taking it as a
/// parameter is what keeps both readable without each pane re-deriving the
/// spacing.
pub(crate) fn tab_header_at<T: Copy + Eq>(
    active: T,
    tabs: &[(T, &str)],
    margin: &'static str,
) -> Line<'static> {
    let mut spans = vec![Span::raw(margin)];
    for (index, (value, label)) in tabs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let style = if *value == active {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::DarkGray)
        };
        let text = if *value == active {
            format!("[{label}]")
        } else {
            (*label).to_string()
        };
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// Build a footer key-hint line from key-action pairs.
pub(crate) fn key_hint(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans: Vec<Span> = Vec::new();
    for (i, (key, action)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" \u{00b7} ", Style::new().fg(Color::DarkGray)));
        }
        spans.push(Span::styled(
            key.to_string(),
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" to {action}"),
            Style::new().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

/// Draw a fixed block above a key row that keeps its place at the bottom. A
/// narrow terminal wraps the keys onto another row rather than dropping the
/// last of them, and a short one shortens the block instead of the key row.
///
/// The block scrolls when it is taller than the room it has, and the position
/// it is held at comes back through the same cell: a block that fits reads as
/// zero, so a downward key at the end of a short block leaves the next upward
/// key something to undo. The limit is what the paragraph's own wrap draws,
/// not a count of how many widths of text the block holds: a word-wrapped
/// line can take one row more than its columns divide into, and a limit short
/// by that row leaves the last line of the block out of reach.
///
/// The scroll hint belongs to the row only while there is something to scroll
/// to. A key named on a block that fits moves nothing, and a row that names it
/// there teaches the user a key that does not work.
pub(crate) fn block_with_keys(
    f: &mut Frame,
    area: Rect,
    body: Vec<Line<'static>>,
    keys: &[(&'static str, &'static str)],
    scroll: &Cell<u16>,
) {
    let mut pairs = keys.to_vec();
    let mut keys_line = key_hint(&pairs);
    let mut keys_height = key_row_height(&keys_line, area);
    let room = area.height.saturating_sub(keys_height);
    if max_scroll_offset(&body, area.width, room) > 0 {
        pairs.insert(0, ("Up/Down", "scroll"));
        keys_line = key_hint(&pairs);
        keys_height = key_row_height(&keys_line, area);
    }
    let keys_row = Paragraph::new(keys_line).wrap(Wrap { trim: false });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(keys_height)])
        .split(area);
    let offset = scroll
        .get()
        .min(max_scroll_offset(&body, chunks[0].width, chunks[0].height));
    scroll.set(offset);
    f.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((offset, 0)),
        chunks[0],
    );
    f.render_widget(keys_row.style(Style::new().fg(Color::DarkGray)), chunks[1]);
}

/// The rows a key row occupies at this width, keeping at least two thirds of
/// the pane for what the keys act on. Every hint stays reachable in the common
/// case by wrapping; only a pane too short to hold both gives up a hint, and
/// the hints are ordered so the last to go is the one for leaving.
pub(crate) fn key_row_height(keys: &Line, area: Rect) -> u16 {
    let needed = line_columns(keys).div_ceil(usize::from(area.width.max(1)));
    needed.clamp(1, usize::from((area.height / 3).max(1))) as u16
}

/// How many terminal columns a line occupies. A wide glyph takes two columns,
/// so counting characters would promise rows the terminal clips.
fn line_columns(line: &Line) -> usize {
    line.spans.iter().map(|span| span.content.width()).sum()
}

#[cfg(test)]
mod tests;
