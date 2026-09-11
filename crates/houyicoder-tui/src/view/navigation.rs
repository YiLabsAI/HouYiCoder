//! Shared pane navigation presentation.
//!
//! Tab headers and key-action footers use one visual grammar so sibling panes
//! do not drift in spacing, active markers, action verbs, or hierarchy hints.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Build a tab row with one bracketed active label.
pub(crate) fn tab_header<T: Copy + Eq>(active: T, tabs: &[(T, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tab_header() {
        let line = tab_header(1, &[(0, "First"), (1, "Second")]);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, "  First  [Second]");
    }

    #[test]
    fn test_hint_pairs() {
        let line = key_hint(&[("Up/Down", "select"), ("Enter", "open"), ("Esc", "close")]);
        assert!(line.spans.len() >= 6, "3 pairs x 2 spans + 2 separators");
    }

    #[test]
    fn test_hint_single() {
        let line = key_hint(&[("Esc", "back")]);
        assert_eq!(line.spans.len(), 2, "one key + one action");
    }

    #[test]
    fn test_hint_empty() {
        let line = key_hint(&[]);
        assert!(line.spans.is_empty(), "no pairs = empty line");
    }
}
