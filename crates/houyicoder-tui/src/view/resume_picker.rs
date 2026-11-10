//! Session picker content for /resume: a header carrying the live filter, one
//! line per session, and a dim topic line under the selection when the
//! record says something the title does not. The state machine and the keys
//! live in the picker state and key modules; this module is presentation.

pub mod state;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};

use crate::resume_picker::SessionRow;
use crate::resume_picker::age_column;
use crate::resume_picker::format_size;
use crate::state::App;
use crate::view::line_wrap::truncate_width;
use crate::view::navigation::key_hint;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use unicode_width::UnicodeWidthStr;

/// Maximum rows the list shows before scrolling. Keeps the pane small so it
/// never covers the whole working surface (the design ~10 visible).
pub const MAX_VISIBLE: usize = 10;

/// Render the resume picker content into the Pane inner rect (the closure
/// passed to draw_command_pane). Header + filtered list + footer.
pub fn draw_content(f: &mut Frame, inner: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);
    // The header repeats the live filter query and states how many sessions
    // this process cannot open now, so a hidden row is not a silent absence.
    let mut spans = vec![
        Span::styled(
            "Resume a session",
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" | filter: {}", app.ui.navigation.resume_picker.query),
            Style::new().fg(Color::DarkGray),
        ),
    ];
    if app.ui.navigation.resume_picker.hidden > 0 {
        spans.push(Span::styled(
            format!(" | {} unavailable", app.ui.navigation.resume_picker.hidden),
            Style::new().fg(Color::DarkGray),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), chunks[0]);
    let filtered = app.ui.navigation.resume_picker.filtered();
    if filtered.is_empty() {
        f.render_widget(
            Paragraph::new("  no session matches your filter")
                .style(Style::new().fg(Color::DarkGray)),
            chunks[1],
        );
    } else {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let selected = app
            .ui
            .navigation
            .resume_picker
            .sel
            .min(filtered.len().saturating_sub(1));
        let items: Vec<ListItem> = filtered
            .iter()
            .enumerate()
            .map(|(i, r)| format_row(r, now, chunks[1].width, i == selected))
            .collect();
        let mut state = ListState::default();
        state.select(Some(selected));
        // The row draws its own marker and highlight: the list's highlight
        // style paints an item's whole area, which would swallow the dim
        // topic line the selected row carries.
        let list = List::new(items).style(Style::default().fg(Color::White));
        f.render_stateful_widget(list, chunks[1], &mut state);
    }
    // Footer: the key hints.
    let footer = Paragraph::new(key_hint(&[
        ("Up/Down", "select"),
        ("Enter", "resume"),
        ("Esc", "close"),
    ]))
    .style(Style::new().fg(Color::DarkGray));
    f.render_widget(footer, chunks[2]);
}

/// One row: selection marker, compact time, size, title in a fixed column,
/// and the cwd basename dim. The title is padded to its budget so the cwd
/// starts at a stable column across rows.
///
/// The selected row gains a dim topic line when its record says something
/// the title does not. Only the selected row pays the extra line, so the
/// list stays one line per session as the user scrolls.
fn format_row(row: &SessionRow, now: u64, width: u16, selected: bool) -> ListItem<'static> {
    let line = format_line(row, now, width, selected);
    let topic = if selected { row.topic_line() } else { None };
    match topic {
        Some(text) => ListItem::new(vec![line, format_topic(&text, width)]),
        None => ListItem::new(line),
    }
}

/// The topic line, indented under the title column and cut to the terminal
/// width like the row above it. It stays dim on the selected row: it is the
/// secondary fact, not part of the highlight.
fn format_topic(text: &str, width: u16) -> Line<'static> {
    let indent = " ".repeat(title_column_offset());
    let room = (width as usize).saturating_sub(indent.chars().count());
    Line::from(vec![
        Span::raw(indent),
        Span::styled(truncate_width(text, room), Style::new().fg(Color::DarkGray)),
    ])
}

/// The column the title starts at inside a row: the marker gutter, the time
/// column, the size column, and their gaps.
fn title_column_offset() -> usize {
    2 + 4 + 1 + 5 + 1
}

fn format_line(row: &SessionRow, now: u64, width: u16, selected: bool) -> Line<'static> {
    let time = age_column(row.last_active, now);
    let size = format_size(row.log_size);
    let budget = title_budget(width);
    let title = truncate_width(&row.title, budget);
    let pad = budget.saturating_sub(UnicodeWidthStr::width(title.as_str()));
    let cwd = truncate_width(&row.cwd_basename, 24);
    let (marker, row_style) = if selected {
        (
            "❯ ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )
    } else {
        ("  ", Style::new().fg(Color::White))
    };
    let dim = if selected {
        row_style
    } else {
        Style::new().fg(Color::DarkGray)
    };
    Line::from(vec![
        Span::styled(marker, row_style),
        Span::styled(format!("{time:>4}"), dim),
        Span::raw(" "),
        Span::styled(format!("{size:>5}"), dim),
        Span::raw(" "),
        Span::styled(title, row_style),
        Span::raw(" ".repeat(pad)),
        Span::raw("  "),
        Span::styled(cwd, dim),
    ])
}

/// The fixed title-column budget: terminal width minus the marker gutter, the
/// time and size columns, the gaps, and the 24-char cwd column, capped at 40
/// so a prompt does not eat the whole row. Floor 8 so a narrow terminal still
/// shows a sliver of title.
fn title_budget(width: u16) -> usize {
    let cwd_column = 24;
    let gap_before_cwd = 2;
    (width as usize)
        .saturating_sub(title_column_offset() + gap_before_cwd + cwd_column)
        .clamp(8, 40)
}

#[cfg(test)]
mod tests;
