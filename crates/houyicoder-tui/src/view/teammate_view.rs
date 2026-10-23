//! Teammate-view banner: the 1-line header naming the viewed agent plus a
//! shift-arrow return hint, drawn above the swapped transcript. The banner
//! is a plain "Viewing" label, the agent name in a distinct style, a dim
//! separator, and the return hint. A second dim line carries the
//! delegation summary.

pub(crate) mod state;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::state::App;
use crate::view::badge_color;

/// Draw the teammate-view banner. No-op when no teammate is in view. The
/// banner is two lines: the title "Viewing @type · shift+↑↓ return" with
/// the agent name bold and tinted by its badge color, then a dim line
/// carrying the delegation summary. A blank margin follows, reserved by
/// the layout so the title sits clear of the transcript.
pub fn draw_banner(f: &mut Frame, app: &App, area: Rect) {
    let Some(view) = app.ui.teammate_view.as_ref() else {
        return;
    };
    let name = if view.subagent_type.is_empty() {
        "agent"
    } else {
        view.subagent_type.as_str()
    };
    let mut name_style = Style::default().add_modifier(Modifier::BOLD);
    if let Some(fg) = view.color.as_deref().and_then(badge_color) {
        name_style = name_style.fg(fg);
    }
    // The hint states what the keys do here, which depends on whether the
    // child still has a turn to interrupt: Esc cancels a running turn and
    // leaves an idle one, so the banner says which.
    let running = app
        .fleet_entry(&view.child_sid)
        .is_some_and(|e| e.completed.is_none());
    let hint = if running {
        " (running) · Esc interrupt · shift+↑↓ return"
    } else {
        " (completed) · Esc return · shift+↑↓ return"
    };
    let title = Line::from(vec![
        Span::raw("Viewing "),
        Span::styled(format!("@{name}"), name_style),
        Span::styled(hint, Style::default().add_modifier(Modifier::DIM)),
    ]);
    // The task line is one row: the prompt can carry newlines and markdown
    // headings, and either would push the hint off the banner or draw a
    // heading where the user expects prose.
    let prompt =
        Line::from(one_line(&view.prompt)).style(Style::default().add_modifier(Modifier::DIM));
    f.render_widget(Paragraph::new(vec![title, prompt]), area);
}

#[cfg(test)]
mod tests;

/// Flatten a task prompt to one line: newlines become spaces and leading
/// markdown heading marks are dropped, so the banner stays a banner.
fn one_line(prompt: &str) -> String {
    prompt
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            let heading = trimmed.trim_start_matches('#');
            // A run of # followed by a space is a heading; a bare # is text.
            if heading.len() < trimmed.len() && heading.starts_with(' ') {
                heading.trim()
            } else {
                trimmed
            }
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}
