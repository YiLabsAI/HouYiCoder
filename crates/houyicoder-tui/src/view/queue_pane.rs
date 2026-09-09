//! The queue pane body: one bounded preview row per logical queued item.
//!
//! Each pending item renders as a single line showing the first line of text
//! plus a hidden-line count for multiline messages. Enter recalls only the
//! selected item; Esc closes the pane; the explicit recall-all action is a
//! separate labeled footer key, not the collapsed-summary click behavior.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};

use crate::pending_queue::PendingItem;
use crate::state::App;

/// The queue pane body: a header line, a scrollable list of pending items
/// (one row per logical item), and a footer hint line.
pub(super) fn draw_content(f: &mut Frame, area: Rect, app: &App) {
    let items: Vec<&PendingItem> = app.pending.iter().collect();
    let n = items.len();
    let cursor = app.queue_view.cursor.min(n.saturating_sub(1));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    f.render_widget(
        Paragraph::new(Line::from(format!(
            "queue — {n} item{}",
            if n == 1 { "" } else { "s" }
        )))
        .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        chunks[0],
    );
    if items.is_empty() {
        f.render_widget(
            Paragraph::new("  (queue is empty)").style(Style::new().fg(Color::DarkGray)),
            chunks[1],
        );
    } else {
        let list_items = queue_items(&items, cursor);
        let mut state = ListState::default();
        state.select(Some(cursor));
        f.render_stateful_widget(
            List::new(list_items)
                .highlight_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("❯ "),
            chunks[1],
            &mut state,
        );
    }
    f.render_widget(
        Paragraph::new(Line::from(
            "  Enter recall \u{00b7} R recall all \u{00b7} d delete \u{00b7} Esc close",
        ))
        .style(Style::new().fg(Color::DarkGray)),
        chunks[2],
    );
}

/// Build the list items: one row per logical queued item, showing the first
/// line plus a hidden-line count for multiline messages.
fn queue_items(items: &[&PendingItem], cursor: usize) -> Vec<ListItem<'static>> {
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let is_cursor = i == cursor;
            let text = item.display();
            let (first_line, hidden) = split_first_line(text);
            let style = if is_cursor {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::White)
            };
            let mut spans: Vec<Span> = vec![Span::styled(first_line.to_string(), style)];
            if hidden > 0 {
                spans.push(Span::styled(
                    format!(" … +{hidden} lines"),
                    Style::new().fg(Color::DarkGray),
                ));
            }
            if let PendingItem::Command(_) = item {
                spans.insert(0, Span::styled("cmd ", Style::new().fg(Color::DarkGray)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect()
}

/// Split text into the first line and a count of remaining lines.
fn split_first_line(text: &str) -> (&str, usize) {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("");
    let hidden = lines.count();
    (first, hidden)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_first_line_single() {
        let (first, hidden) = split_first_line("hello");
        assert_eq!(first, "hello");
        assert_eq!(hidden, 0);
    }

    #[test]
    fn test_split_first_line_multi() {
        let (first, hidden) = split_first_line("line one\nline two\nline three");
        assert_eq!(first, "line one");
        assert_eq!(hidden, 2);
    }
}
