//! The startup workspace-trust banner: a full-width top banner shown as
//! the sole pre-chat setup screen while a trust ask is in flight (the main
//! view is not drawn until the user answers, not a popup over a live chat).
//! Simpler than the approval card (no tool input, no focus, two keys)
//! because trust is a one-time workspace ack, not a per-tool verdict.
//! Enter or y trusts; Esc or n declines (which ends the session).

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};

use crate::state::App;

/// Render the trust prompt as a full-width top banner when a trust ask is
/// pending. The main view is not drawn while trust is pending (see
/// view::draw), so this is the sole setup screen — not a centered popup
/// overlaying a live chat. No-op when no ask is in flight (the common case
/// — most sessions are already trusted or non-project).
pub fn draw(f: &mut Frame, app: &App) {
    let Some(prompt) = app.pending_trust.as_ref() else {
        return;
    };
    let area = f.area();
    // One-row top margin, then the banner, then the rest. The banner is
    // full-width and in-flow — no Clear, no centered popup.
    let banner = Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(11),
            Constraint::Min(0),
        ])
        .split(area)[1];
    let block = Block::default()
        .borders(Borders::TOP)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Yellow))
        .title({
            Line::from(Span::styled(
                " Accessing workspace ",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ))
        })
        .title_alignment(Alignment::Center);
    let inner = block.inner(banner);
    f.render_widget(block, banner);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(" {}", prompt.project_path),
        Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(
        " Quick safety check: is this a project you created or one you trust?",
    ));
    lines.push(Line::from(
        " The agent will be able to read, edit, and execute files here.",
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Enter  Trust this project",
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        "  Esc    Decline (exit)",
        Style::new().fg(Color::DarkGray),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use crate::composition;
    use crate::test_support::render_text;

    /// A pending trust ask renders the project path + the two-button prompt,
    /// so the user sees which folder is asking + how to answer.
    #[test]
    fn test_renders_trust_prompt() {
        let mut app = composition::app();
        app.screen = crate::state::Screen::Working;
        app.pending_trust = Some(houyicoder_protocol::frontend::trust::TrustPrompt {
            project_path: "/home/alice/proj".into(),
            risks: Vec::new(),
        });
        let out = render_text(&app, 80, 24);
        assert!(out.contains("Accessing workspace"), "title missing:\n{out}");
        assert!(
            out.contains("/home/alice/proj"),
            "project path missing:\n{out}"
        );
        assert!(
            out.contains("Trust this project"),
            "accept hint missing:\n{out}"
        );
        assert!(out.contains("Decline"), "decline hint missing:\n{out}");
    }

    /// No pending trust ask: draw is a no-op (the common case), so the
    /// trust banner must not appear.
    #[test]
    fn test_no_render_without_ask() {
        let mut app = composition::app();
        app.screen = crate::state::Screen::Working;
        let out = render_text(&app, 80, 24);
        assert!(
            !out.contains("Accessing workspace"),
            "trust banner must not render with no ask pending:\n{out}"
        );
    }

    /// While a trust ask is pending, the main view is not drawn — trust is
    /// the sole setup screen, not a popup over a live chat. The working
    /// surface input placeholder is absent; only the banner renders.
    #[test]
    fn test_trust_is_sole_screen() {
        let mut app = composition::app();
        app.screen = crate::state::Screen::Working;
        app.pending_trust = Some(houyicoder_protocol::frontend::trust::TrustPrompt {
            project_path: "/home/alice/proj".into(),
            risks: Vec::new(),
        });
        let out = render_text(&app, 80, 24);
        assert!(
            out.contains("Accessing workspace"),
            "banner present:\n{out}"
        );
        assert!(
            !out.contains("let's build"),
            "main view must not draw while trust pending:\n{out}"
        );
    }
}
