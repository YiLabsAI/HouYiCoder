//! The /agents pane: live fleet when children are running, the registered
//! agent directory when idle. Split from capability.rs so that file stays
//! under the size gate.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Text},
    widgets::{List, ListItem, Paragraph, Wrap},
};

use crate::state::App;
use crate::view::capability::titled_block;

/// Rows the pane reserves when opened inline by the slash command.
pub(crate) const AGENTS_PANE_HEIGHT: u16 = 14;

/// Render the agents pane inside a titled block. Used by the capability grid,
/// which hands over a bare rect; the inline slash-command pane draws its own
/// frame and calls draw_content with the inner rect instead.
pub(crate) fn draw_agents(f: &mut Frame, area: Rect, app: &App) {
    let block = titled_block(app, "agents");
    let inner = block.inner(area);
    f.render_widget(block, area);
    draw_content(f, inner, app);
}

/// The pane's content rows: one row per live child with type, status, tokens,
/// and turn, the selected one marked; an idle fleet falls back to the
/// directory the query fetched, or a placeholder while the reply is in
/// flight.
pub(crate) fn draw_content(f: &mut Frame, area: Rect, app: &App) {
    let entries = &app.sessions.current.capabilities.fleet.entries;
    if !entries.is_empty() {
        let selected = app.ui.fleet.selected;
        let items: Vec<ListItem> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let status = match &e.completed {
                    Some(s) => s.as_str(),
                    None => "running",
                };
                let row = format!(
                    "{} · {} · {} tok · turn {}",
                    e.subagent_type, status, e.tokens, e.turn,
                );
                if selected == Some(i) {
                    ListItem::new(format!("▶ {row}"))
                } else {
                    ListItem::new(format!("  {row}"))
                }
            })
            .collect();
        f.render_widget(List::new(items).style(Style::new().fg(Color::White)), area);
        return;
    }
    // Returned delegations outlive the footer strip: the strip drops a
    // completed child after its grace window, but the pane is the record of
    // what ran this session, selectable and enterable.
    if !app.sessions.current.agents.rows.is_empty() {
        let rows = &app.sessions.current.agents.rows;
        let cur = app.ui.agents_sel.min(rows.len().saturating_sub(1));
        let items: Vec<ListItem> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let row = format!("\u{2713} {} \u{00b7} {}", r.subagent_type, r.summary);
                if cur == i {
                    ListItem::new(format!("\u{25b6} {row}"))
                } else {
                    ListItem::new(format!("  {row}"))
                }
            })
            .collect();
        f.render_widget(List::new(items).style(Style::new().fg(Color::White)), area);
        return;
    }
    // Nothing live and nothing returned yet: the registered directory.
    let dir = app
        .sessions
        .current
        .agent_directory
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("(no agent directory loaded)");
    let lines: Vec<Line> = dir.lines().map(|l| Line::from(l.to_string())).collect();
    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        area,
    );
}

#[cfg(test)]
mod tests;
