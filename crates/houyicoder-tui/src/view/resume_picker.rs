//! Session picker content for /resume. Renders into the shared Pane template
//! (draw_command_pane) below the transcript tail: a header row with the live
//! filter, a session list (relative time + title + cwd basename, no sid), and
//! a dim footer. The state machine (SessionPickerState: open / sel / query /
//! filtered) and the keys (Up / Down / Enter / Esc / char) live in
//! resume_picker.rs + keys.rs; this module is pure presentation. A
//! log-selector shape (a filtered list, not a modal) at a simplified
//! density (sid OR title substring, no fuse / no preview / no cross-project).

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};

use crate::state::App;

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
            format!(" | filter: {}", app.resume_picker.query),
            Style::new().fg(Color::DarkGray),
        ),
    ]));
    f.render_widget(header, chunks[0]);
    let filtered = app.resume_picker.filtered();
    if filtered.is_empty() {
        f.render_widget(
            Paragraph::new("  no session matches your filter")
                .style(Style::new().fg(Color::DarkGray)),
            chunks[1],
        );
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let items: Vec<ListItem> = filtered
            .iter()
            .map(|r| format_row(r, now, chunks[1].width))
            .collect();
        let mut state = ListState::default();
        state.select(Some(
            app.resume_picker.sel.min(filtered.len().saturating_sub(1)),
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
    let footer = Paragraph::new("Up/Down=move Enter=resume Esc=close")
        .style(Style::new().fg(Color::DarkGray));
    f.render_widget(footer, chunks[2]);
}

/// One row: compact time, title (truncated with ellipsis), cwd basename dim.
fn format_row(row: &crate::resume_picker::SessionRow, now: u64, width: u16) -> ListItem<'static> {
    ListItem::new(format_line(row, now, width))
}

/// The Line one row renders, extracted so a unit test can assert the time +
/// title + cwd basename (and the sid's absence) without poking private
/// widget fields.
fn format_line(row: &crate::resume_picker::SessionRow, now: u64, width: u16) -> Line<'static> {
    let time = crate::resume_picker::relative_time(row.last_active, now);
    let title_max = title_max(width);
    let title = truncate(&row.title, title_max);
    let cwd = truncate(&row.cwd_basename, 24);
    Line::from(vec![
        Span::styled(format!("{time:>4} "), Style::new().fg(Color::DarkGray)),
        Span::styled(title, Style::new().fg(Color::White)),
        Span::raw("  "),
        Span::styled(cwd, Style::new().fg(Color::DarkGray)),
    ])
}

fn title_max(width: u16) -> usize {
    // time(5) + cwd(24) + separators(4) + highlight symbol(2)
    (width as usize).saturating_sub(5 + 24 + 4 + 2).max(8)
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max <= 3 {
        return s.chars().take(max).collect();
    }
    let kept: String = s.chars().take(max - 3).collect();
    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row renders the time, the title, and the cwd basename, and never the
    /// sid (the design density decision).
    #[test]
    fn test_format_row_layout() {
        let row = crate::resume_picker::SessionRow {
            sid_str: "secret-sid".into(),
            title: "login flow".into(),
            cwd_basename: "app".into(),
            last_active: 120,
            ..Default::default()
        };
        let line = format_line(&row, 240, 80);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("2m"), "time column: {rendered}");
        assert!(rendered.contains("login flow"), "title: {rendered}");
        assert!(rendered.contains("app"), "cwd basename: {rendered}");
        assert!(
            !rendered.contains("secret-sid"),
            "sid must not show: {rendered}"
        );
    }

    /// Truncation appends an ellipsis when the title exceeds the column budget.
    #[test]
    fn test_truncate_appends_ellipsis() {
        let s = "abcdefghij";
        assert_eq!(truncate(s, 5), "ab...");
        assert_eq!(truncate(s, 10), "abcdefghij");
        assert_eq!(truncate(s, 0), "");
    }
}
