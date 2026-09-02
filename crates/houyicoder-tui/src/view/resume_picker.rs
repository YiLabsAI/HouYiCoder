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
use crate::view::line_wrap::truncate_width;

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

/// One row: compact time, title (fixed-width column so the cwd column
/// aligns across rows), cwd basename dim. The title is padded to its budget
/// so the cwd starts at a stable column — a one-line layout that stays
/// scannable where a two-line layout would spend the extra row.
fn format_row(row: &crate::resume_picker::SessionRow, now: u64, width: u16) -> ListItem<'static> {
    ListItem::new(format_line(row, now, width))
}

fn format_line(row: &crate::resume_picker::SessionRow, now: u64, width: u16) -> Line<'static> {
    let time = crate::resume_picker::relative_time(row.last_active, now);
    let budget = title_budget(width);
    let title = truncate_width(&row.title, budget);
    let pad = budget.saturating_sub(unicode_width::UnicodeWidthStr::width(title.as_str()));
    let cwd = truncate_width(&row.cwd_basename, 24);
    Line::from(vec![
        Span::styled(format!("{time:>4}"), Style::new().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(title, Style::new().fg(Color::White)),
        Span::raw(" ".repeat(pad)),
        Span::raw("  "),
        Span::styled(cwd, Style::new().fg(Color::DarkGray)),
    ])
}

/// The fixed title-column budget: terminal width minus the time column, the
/// two 2-space gaps, and the 24-char cwd column, capped at 40 so a long slug
/// does not eat the whole row. Floor 8 so a narrow terminal still shows a
/// sliver of title.
fn title_budget(width: u16) -> usize {
    // time(4) + gap(2) + title + gap(2) + cwd(24) + highlight symbol(2)
    (width as usize)
        .saturating_sub(4 + 2 + 2 + 24 + 2)
        .clamp(8, 40)
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
        assert_eq!(truncate_width(s, 10), "abcdefghij");
        assert_eq!(truncate_width(s, 0), "");
        let out = truncate_width(s, 5);
        assert!(out.ends_with('\u{2026}'));
        assert!(
            unicode_width::UnicodeWidthStr::width(out.as_str()) <= 5,
            "width overflow: {out}"
        );
    }
}
