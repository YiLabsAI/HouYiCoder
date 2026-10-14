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
mod tests;
