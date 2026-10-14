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
    let Some(prompt) = app.pending_trust() else {
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
            app.trust_choice() == TrustChoice::Accept,
            "Yes, I trust this folder",
        ),
        option_line(app.trust_choice() == TrustChoice::Exit, "No, exit"),
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
mod tests;
