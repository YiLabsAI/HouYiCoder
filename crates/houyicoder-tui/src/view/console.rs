//! Review-node console (review and audit): NOT a PR dashboard, but a review/audit surface.
//! Reached after SSO/API-key login or via /console from the working surface.
//! Left column = review queue: each finding carries a verdict, severity, an
//! evidence trail (hunk + spec clause + test + adversarial summary), a replay
//! button (stub), and sign-off / reject affordances. Right column = audit
//! trail: signed-off/rejected findings with who/when/replayable and
//! a stub hash-chain projection. All data is placeholder.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, List, ListItem, Paragraph, Wrap},
};

use crate::state::{App, Verdict};
use crate::view::components;

/// Render the review-node console.
pub fn draw(f: &mut Frame, app: &App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_topbar(f, outer[0], app);
    draw_columns(f, outer[1], app);
    f.render_widget(
        Paragraph::new(" Enter: start a coding task | Up/Down: finding | a: approve | r: reject | p: replay | Esc: quit ")
            .style(Style::new().fg(Color::DarkGray)),
        outer[2],
    );
}

fn draw_topbar(f: &mut Frame, area: Rect, app: &App) {
    let mode = match app.login_mode {
        Some(houyicoder_protocol::frontend::LoginMode::Sso) => "SSO",
        Some(houyicoder_protocol::frontend::LoginMode::ApiKey) => "API key",
        Some(houyicoder_protocol::frontend::LoginMode::Local) => "local",
        None => "-",
    };
    let text = format!(
        " review console | auth:{} | org:stub | workspace:hicoder | model:{} | sandbox:{} ",
        mode, app.status.model, app.status.sandbox,
    );
    f.render_widget(
        Paragraph::new(text).style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        area,
    );
}

fn draw_columns(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(area);
    draw_review_queue(f, cols[0], app);
    draw_audit_trail(f, cols[1], app);
}

fn draw_review_queue(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" review queue (approve / reject) ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(0)])
        .split(inner);
    draw_queue_summary(f, split[0], app);
    draw_finding_detail(f, split[1], app);
}

fn draw_queue_summary(f: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .review
        .findings
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let marker = if i == app.review.focus { "> " } else { "  " };
            let tag = match r.signoff {
                Verdict::Pending => "",
                Verdict::Approved => " [ok]",
                Verdict::Rejected => " [rej]",
            };
            ListItem::new(format!(
                "{}{} [{}] verdict:{} sev:{}{}",
                marker, r.id, r.lens, r.verdict, r.severity, tag
            ))
        })
        .collect();
    f.render_widget(List::new(items).style(Style::new().fg(Color::White)), area);
}

fn draw_finding_detail(f: &mut Frame, area: Rect, app: &App) {
    let Some(r) = app
        .review
        .findings
        .get(app.review.focus.min(app.console_len().saturating_sub(1)))
    else {
        f.render_widget(
            Paragraph::new("(no findings)").style(Style::new().fg(Color::DarkGray)),
            area,
        );
        return;
    };
    let white = Style::new().fg(Color::White);
    let dim = Style::new().fg(Color::Gray);
    let lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(
                format!("finding {}  ", r.id),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("[{}]  ", r.lens), white),
            Span::styled(
                format!("verdict: {}  ", r.verdict),
                components::verdict_style(&r.verdict),
            ),
            Span::styled(
                format!("severity: {}", r.severity),
                components::severity_style(&r.severity),
            ),
        ]),
        Line::from(format!(
            "  evidence: {} ({}) | spec {} | test {}",
            r.hunk_id, r.note, r.spec_clause_id, r.test_id
        ))
        .style(white),
        components::consensus_line(&app.review.findings),
        Line::from(format!("  replay: [p] jump to {} ", r.hunk_id)).style(dim),
        Line::from(""),
        components::signoff_row(r.signoff),
    ];
    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_audit_trail(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" decision log (hash-chain projection) ");
    let items: Vec<ListItem> = app
        .review
        .audit_trail
        .iter()
        .map(|e| {
            ListItem::new(format!(
                "{} {} [{}] by {} @ {} | {}",
                e.hash, e.finding_id, e.verdict, e.who, e.when, e.replay_ref
            ))
        })
        .collect();
    f.render_widget(
        List::new(items).style(Style::new().fg(Color::White)),
        block.inner(area),
    );
    f.render_widget(block, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_console_uses_shared_verdict() {
        // The console delegates to the shared component, so the real verdict
        // is Cyan (not Red) under the monochrome vocabulary.
        assert_eq!(components::verdict_style("real").fg, Some(Color::Cyan));
        assert_eq!(
            components::verdict_style("refuted").fg,
            Some(Color::DarkGray)
        );
    }
}
