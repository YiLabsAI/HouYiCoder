//! The startup workspace-trust card: a centered popup rendered while a
//! pending trust ask is in flight. Simpler than the approval card (no tool
//! input, no focus, two buttons) because trust is a one-time workspace ack,
//! not a per-tool verdict. Enter or y trusts; Esc or n declines (which ends
//! the session on the server side).

use ratatui::{
    Frame,
    layout::Alignment,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};

use crate::state::App;

/// Render the trust prompt as a centered popup when a trust ask is pending.
/// No-op when no ask is in flight (the common case — most sessions are
/// already trusted or non-project).
pub fn draw(f: &mut Frame, app: &App) {
    let Some(prompt) = app.pending_trust.as_ref() else {
        return;
    };
    let area = f.area();
    let popup = super::centered(60, 40, area);
    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Yellow))
        .title({
            Line::from(Span::styled(
                " Accessing workspace ",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ))
        })
        .title_alignment(Alignment::Center);
    let inner = block.inner(popup);
    f.render_widget(block, popup);

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
    /// trust popup must not appear.
    #[test]
    fn test_no_render_without_ask() {
        let mut app = composition::app();
        app.screen = crate::state::Screen::Working;
        let out = render_text(&app, 80, 24);
        assert!(
            !out.contains("Accessing workspace"),
            "trust popup must not render with no ask pending:\n{out}"
        );
    }
}
