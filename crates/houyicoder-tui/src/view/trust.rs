//! The startup workspace-trust screen shown before chat begins.
//! Enter accepts the project boundary; Esc declines and exits.

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};

use crate::list_pane_state::truncate_path;
use crate::state::{App, TrustChoice};

/// Render the pending trust decision as the only startup surface.
pub fn draw(f: &mut Frame, app: &App) {
    let Some(prompt) = app.pending_trust.as_ref() else {
        return;
    };
    let card = centered_card(f.area());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Span::styled(
            " Trust this workspace? ",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Center);
    let inner = block.inner(card);
    f.render_widget(block, card);

    let path_width = usize::from(inner.width.saturating_sub(2));
    let path = truncate_path(&prompt.project_path, path_width);
    let lines = vec![
        Line::from(Span::styled(
            format!(" {path}"),
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        Line::default(),
        Line::from(" Quick safety check: Is this a project you created or trust?"),
        Line::from(" If not, review the folder before continuing."),
        Line::from(" The agent can read, edit, and execute files here."),
        Line::default(),
        option_line(
            app.trust_choice == TrustChoice::Accept,
            "Yes, I trust this folder",
        ),
        option_line(app.trust_choice == TrustChoice::Exit, "No, exit"),
        Line::from(Span::styled(
            " Enter to confirm · Esc to cancel",
            Style::new().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

fn centered_card(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).min(76);
    let height = area.height.min(11);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn option_line(selected: bool, label: &'static str) -> Line<'static> {
    let marker = if selected { " › " } else { "   " };
    let style = if selected {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::White)
    };
    Line::from(Span::styled(format!("{marker}{label}"), style))
}

#[cfg(test)]
mod tests {
    use super::TrustChoice;
    use houyicoder_protocol::frontend::trust::TrustPrompt;

    use crate::composition;
    use crate::state::Screen;
    use crate::test_support::render_text;

    /// A pending trust ask renders the project path + the two-button prompt,
    /// so the user sees which folder is asking + how to answer.
    #[test]
    fn test_renders_trust_prompt() {
        let mut app = composition::app();
        app.screen = Screen::Working;
        app.pending_trust = Some(TrustPrompt {
            project_path: "/home/alice/proj".into(),
            risks: Vec::new(),
        });
        let out = render_text(&app, 80, 24);
        assert!(
            out.contains("Trust this workspace?"),
            "title missing:\n{out}"
        );
        assert!(
            out.contains("/home/alice/proj"),
            "project path missing:\n{out}"
        );
        assert!(
            out.contains("Yes, I trust this folder"),
            "accept hint missing:\n{out}"
        );
        assert!(out.contains("No, exit"), "decline hint missing:\n{out}");
        assert!(out.contains("› Yes, I trust this folder"));
        app.trust_choice = TrustChoice::Exit;
        let out = render_text(&app, 80, 24);
        assert!(out.contains("› No, exit"));
    }

    #[test]
    fn test_long_path_stays_single() {
        let mut app = composition::app();
        app.pending_trust = Some(TrustPrompt {
            project_path: "/a/very/long/workspace/path/that/cannot/fit/target-repository".into(),
            risks: Vec::new(),
        });
        let out = render_text(&app, 40, 18);
        assert!(out.contains('…'), "long path is visibly truncated: {out}");
        assert!(
            out.contains("target-repository"),
            "path tail survives: {out}"
        );
        assert_eq!(
            out.lines()
                .filter(|line| line.contains("target-repository"))
                .count(),
            1,
            "path stays on one row: {out}"
        );
    }

    /// No pending trust ask: draw is a no-op (the common case), so the
    /// trust banner must not appear.
    #[test]
    fn test_no_render_without_ask() {
        let mut app = composition::app();
        app.screen = Screen::Working;
        let out = render_text(&app, 80, 24);
        assert!(
            !out.contains("Trust this workspace?"),
            "trust banner must not render with no ask pending:\n{out}"
        );
    }

    /// While a trust ask is pending, the main view is not drawn — trust is
    /// the sole setup screen, not a popup over a live chat. The working
    /// surface input placeholder is absent; only the banner renders.
    #[test]
    fn test_trust_is_sole_screen() {
        let mut app = composition::app();
        app.screen = Screen::Working;
        app.pending_trust = Some(TrustPrompt {
            project_path: "/home/alice/proj".into(),
            risks: Vec::new(),
        });
        let out = render_text(&app, 80, 24);
        assert!(
            out.contains("Trust this workspace?"),
            "banner present:\n{out}"
        );
        assert!(
            !out.contains("let's build"),
            "main view must not draw while trust pending:\n{out}"
        );
    }
}
