//! Shared footer key-hint builder. Produces a one-line hint where each key
//! is bold-dim and each action is dim, separated by a middle dot — matching
//! the idiomatic style where the key stands out and the action recedes.
//! Callers pass (key, action) pairs; the helper handles the formatting
//! so every pane renders hints the same way.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Build a footer key-hint line from key-action pairs.
/// Each pair renders as key to action; pairs are joined by a middle dot.
/// The key is bold dim (stands out), the action is dim (recedes).
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
