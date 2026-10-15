//! Session picker content for /resume. Renders into the shared Pane template
//! (draw_command_pane) below the transcript tail: a header row with the live
//! filter, a session list (relative time + title + cwd basename, no sid), and
//! a dim footer. The state machine (SessionPickerState: open / sel / query /
//! filtered) and the keys (Up / Down / Enter / Esc / char) live in
//! resume_picker.rs + keys.rs; this module is pure presentation. A
//! log-selector shape (a filtered list, not a modal) at a simplified
//! density (sid OR title substring, no fuse / no preview / no cross-project).

pub mod state;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};

use crate::resume_picker::SessionRow;
use crate::resume_picker::format_size;
use crate::resume_picker::relative_time;
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
    // Header: title + live filter query.
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            "Resume a session",
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" | filter: {}", app.ui.navigation.resume_picker.query),
            Style::new().fg(Color::DarkGray),
        ),
    ]));
    f.render_widget(header, chunks[0]);
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
        let items: Vec<ListItem> = filtered
            .iter()
            .map(|r| format_row(r, now, chunks[1].width))
            .collect();
        let mut state = ListState::default();
        state.select(Some(
            app.ui
                .navigation
                .resume_picker
                .sel
                .min(filtered.len().saturating_sub(1)),
        ));
        let list = List::new(items)
            .style(Style::default().fg(Color::White))
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
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

/// One row: compact time, title (fixed-width column so the cwd column
/// aligns across rows), cwd basename dim. The title is padded to its budget
/// so the cwd starts at a stable column — a one-line layout that stays
/// scannable where a two-line layout would spend the extra row.
fn format_row(row: &SessionRow, now: u64, width: u16) -> ListItem<'static> {
    ListItem::new(format_line(row, now, width))
}

fn format_line(row: &SessionRow, now: u64, width: u16) -> Line<'static> {
    let time = relative_time(row.last_active, now);
    let size = format_size(row.log_size);
    let budget = title_budget(width);
    let title = truncate_width(&row.title, budget);
    let pad = budget.saturating_sub(UnicodeWidthStr::width(title.as_str()));
    let cwd = truncate_width(&row.cwd_basename, 24);
    Line::from(vec![
        Span::styled(format!("{time:>4}"), Style::new().fg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(format!("{size:>5}"), Style::new().fg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(title, Style::new().fg(Color::White)),
        Span::raw(" ".repeat(pad)),
        Span::raw("  "),
        Span::styled(cwd, Style::new().fg(Color::DarkGray)),
    ])
}

/// The fixed title-column budget: terminal width minus the time column, the
/// size column, the gaps, and the 24-char cwd column, capped at 40 so a long
/// slug does not eat the whole row. Floor 8 so a narrow terminal still shows
/// a sliver of title.
fn title_budget(width: u16) -> usize {
    // time(4) + gap(1) + size(5) + gap(1) + title + gap(2) + cwd(24) + highlight(2)
    (width as usize)
        .saturating_sub(4 + 1 + 5 + 1 + 2 + 24 + 2)
        .clamp(8, 40)
}

#[cfg(test)]
mod tests;
