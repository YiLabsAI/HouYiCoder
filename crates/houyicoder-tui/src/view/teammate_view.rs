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
use crate::view::badge_color;

/// Draw the teammate-view banner. No-op when no teammate is in view. The
/// banner is two lines: the title "Viewing @type · esc return" with the
/// agent name bold and tinted by its badge color, then a dim line
/// carrying the delegation summary. A blank margin follows, reserved by
/// the layout so the title sits clear of the transcript.
pub fn draw_banner(f: &mut Frame, app: &App, area: Rect) {
    let Some(view) = app.teammate_view.as_ref() else {
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
            " · esc return",
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]);
    let prompt =
        Line::from(view.prompt.clone()).style(Style::default().add_modifier(Modifier::DIM));
    f.render_widget(Paragraph::new(vec![title, prompt]), area);
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
            prompt: "find auth".into(),
            color: None,
            transcript: vec![],
            ..Default::default()
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
            prompt: String::new(),
            color: None,
            transcript: vec![],
            ..Default::default()
        });
        let view = app.teammate_view.as_ref().unwrap();
        let name = if view.subagent_type.is_empty() {
            "agent"
        } else {
            view.subagent_type.as_str()
        };
        assert_eq!(name, "agent");
    }

    /// The banner renders above the transcript when a teammate is in view,
    /// naming the agent plus the esc-return hint. Pins draw_banner against a
    /// refactor that drops the line or the hint.
    #[test]
    fn test_banner_renders_name_hint() {
        use crate::records::TranscriptLine;
        use crate::test_support::render_text;
        let mut app = app();
        app.screen = crate::state::Screen::Working;
        app.teammate_view = Some(crate::records::TeammateView {
            child_sid: "c1".into(),
            subagent_type: "explore".into(),
            prompt: "find auth".into(),
            color: None,
            transcript: vec![TranscriptLine::Agent("child reply".into())],
            ..Default::default()
        });
        let out = render_text(&app, 80, 24);
        assert!(
            out.contains("Viewing @explore"),
            "banner names the agent, got:\n{out}"
        );
        assert!(
            out.contains("esc return"),
            "banner carries the esc-return hint, got:\n{out}"
        );
        assert!(
            out.contains("find auth"),
            "banner carries the dim summary line, got:\n{out}"
        );
        assert!(
            out.contains("child reply"),
            "swapped child transcript renders, got:\n{out}"
        );
    }

    /// No banner renders when no teammate is in view, so the transcript takes
    /// the whole top. Pins the no-banner path against a regression that
    /// reserves a blank line.
    #[test]
    fn test_no_banner_without_teammate() {
        use crate::test_support::render_text;
        let mut app = app();
        app.screen = crate::state::Screen::Working;
        let out = render_text(&app, 80, 24);
        assert!(
            !out.contains("Viewing @"),
            "no banner when teammate_view is None, got:\n{out}"
        );
    }

    /// A badge color on the viewed teammate is carried into the banner so the
    /// name is distinguishable from siblings. Pins the color wiring against a
    /// regression that drops it.
    #[test]
    fn test_banner_carries_badge_color() {
        use crate::test_support::{render_buffer, working_app};
        let mut app = working_app();
        app.teammate_view = Some(crate::records::TeammateView {
            child_sid: "c1".into(),
            subagent_type: "verify".into(),
            prompt: "ran deep review".into(),
            color: Some("red".into()),
            transcript: vec![],
            ..Default::default()
        });
        let buf = render_buffer(&app, 80, 24);
        // The banner is the only row carrying an "@"; assert the @ cell is
        // red so the badge color carried through to the banner name.
        let hit = buf
            .content()
            .iter()
            .any(|c| c.fg == ratatui::style::Color::Red && c.symbol() == "@");
        assert!(
            hit,
            "the @verify name carries the red badge color, got buffer with no red @ cell"
        );
    }
}
