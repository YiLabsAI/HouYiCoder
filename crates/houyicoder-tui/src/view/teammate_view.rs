//! Teammate-view banner: the 1-line header naming the viewed agent plus an
//! esc-return hint, drawn above the swapped transcript. The banner is a
//! plain "Viewing" label, the agent name in a distinct style, a dim
//! separator, and the esc-return hint. A second dim line carries the
//! delegation summary.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::state::App;

/// Draw the teammate-view banner. No-op when no teammate is in view. The
/// banner is a single line: "Viewing @type · esc return". The agent name
/// renders bold; per-agent color wiring lands with the color-badge task,
/// so the name is bold-only here until that lands.
pub fn draw_banner(f: &mut Frame, app: &App, area: Rect) {
    let Some(view) = app.teammate_view.as_ref() else {
        return;
    };
    let name = if view.subagent_type.is_empty() {
        "agent"
    } else {
        view.subagent_type.as_str()
    };
    let line = Line::from(vec![
        Span::raw("Viewing "),
        Span::styled(
            format!("@{name}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" · esc return"),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use crate::composition::app;

    #[test]
    fn test_banner_shows_type() {
        let mut app = app();
        app.teammate_view = Some(crate::records::TeammateView {
            child_sid: "c1".into(),
            subagent_type: "explore".into(),
            summary: "find auth".into(),
            transcript: vec![],
        });
        let view = app.teammate_view.as_ref().unwrap();
        let name = if view.subagent_type.is_empty() {
            "agent"
        } else {
            view.subagent_type.as_str()
        };
        assert_eq!(name, "explore");
    }

    #[test]
    fn test_banner_type_fallback() {
        let mut app = app();
        app.teammate_view = Some(crate::records::TeammateView {
            child_sid: "c2".into(),
            subagent_type: String::new(),
            summary: String::new(),
            transcript: vec![],
        });
        let view = app.teammate_view.as_ref().unwrap();
        let name = if view.subagent_type.is_empty() {
            "agent"
        } else {
            view.subagent_type.as_str()
        };
        assert_eq!(name, "agent");
    }
}
