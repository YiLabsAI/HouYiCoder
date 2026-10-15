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
    let title = Line::from(vec![
        Span::raw("Viewing "),
        Span::styled(format!("@{name}"), name_style),
        Span::styled(
            " · shift+↑↓ return",
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]);
    let prompt =
        Line::from(view.prompt.clone()).style(Style::default().add_modifier(Modifier::DIM));
    f.render_widget(Paragraph::new(vec![title, prompt]), area);
}

#[cfg(test)]
mod tests;
