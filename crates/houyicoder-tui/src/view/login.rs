//! Login screen: V1 medium logo, sign-in prompt, and three options (SSO, API
//! key, local mode). Placeholder auth; picking any proceeds. Local mode skips
//! the console.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
};

use crate::state::App;
use crate::view::logo;

/// Render the login screen.
pub fn draw(f: &mut Frame, app: &App) {
    let area = centered_box(f);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .title(" houyicoder / hicoder ");
    // Use the bordered block's own inner area (inset by the border) so the
    // logo and text render below the title border, not on top of it. A fresh
    // NONE-borders block would return the full area and let the logo's first
    // line overwrite the title.
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            // One blank line between the three options and the nav hint so the
            // gray hint does not sit crammed against the last option (the
            // option block reads as its own group, the hint as an aside).
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    f.render_widget(Paragraph::new(logo::medium()), chunks[0]);
    f.render_widget(
        Paragraph::new("sign in to houyicoder").style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        chunks[1],
    );
    f.render_widget(Paragraph::new(""), chunks[2]);
    f.render_widget(option_line('1', "SSO (enterprise OIDC/SAML)"), chunks[3]);
    f.render_widget(option_line('2', "API key (cloud model)"), chunks[4]);
    f.render_widget(
        option_line('3', "local mode (no login, local model)"),
        chunks[5],
    );
    f.render_widget(Paragraph::new(""), chunks[6]);
    f.render_widget(
        Paragraph::new("press 1/2/3 to continue; Esc to quit")
            .style(Style::default().fg(Color::DarkGray))
            .wrap(Wrap { trim: true }),
        chunks[7],
    );
    let _ = app;
}

fn option_line(key: char, label: &str) -> Paragraph<'static> {
    Paragraph::new(format!("  [{key}] {label}")).style(Style::default().fg(Color::White))
}

fn centered_box(f: &mut Frame) -> Rect {
    let area = f.area();
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(15),
            Constraint::Length(14),
            Constraint::Min(0),
        ])
        .split(area)[1]
}
