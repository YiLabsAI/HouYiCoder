//! The /agents pane: live fleet when children are running, the registered
//! agent directory when idle. Split from capability.rs so that file stays
//! under the size gate.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{List, ListItem},
};

use crate::state::App;
use crate::view::capability::{render_lines, titled_block};

/// Render the agents pane. A non-empty fleet shows one row per child with
/// type, status, tokens, and turn; the selection gets a marker. An idle
/// fleet falls back to the directory the /agents query fetched, or a
/// placeholder when the reply has not landed yet.
pub(super) fn draw_agents(f: &mut Frame, area: Rect, app: &App) {
    let block = titled_block(app, "agents");
    if !app.fleet.entries.is_empty() {
        let items: Vec<ListItem> = app
            .fleet
            .entries
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
                if app.fleet.selected == Some(i) {
                    ListItem::new(format!("▶ {row}"))
                } else {
                    ListItem::new(format!("  {row}"))
                }
            })
            .collect();
        f.render_widget(
            List::new(items).style(Style::new().fg(Color::White)),
            block.inner(area),
        );
        f.render_widget(block, area);
        return;
    }
    if app.agents.is_empty() {
        let dir = app
            .agent_directory
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("(no agent directory loaded)");
        let lines: Vec<Line> = dir.lines().map(|l| Line::from(l.to_string())).collect();
        render_lines(f, area, block, lines);
        return;
    }
    let items: Vec<ListItem> = app
        .agents
        .iter()
        .map(|a| ListItem::new(format!("{} ({}) -- {}", a.name, a.role, a.state)))
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
    use crate::agent_message::FleetEntry;
    use crate::composition;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| {
                draw_agents(f, f.area(), app);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// An idle fleet with a fetched directory renders the directory header,
    /// not the placeholder. Pins that the AgentsResult reply reaches the
    /// pane and the directory text lands on screen.
    #[test]
    fn test_directory_renders_when_fetched() {
        let mut app = composition::app();
        app.agent_directory = Some("## Available agents\n\n- explore: fast".into());
        let content = render(&app, 40, 6);
        assert!(
            content.contains("Available agents"),
            "directory header should render: {content}"
        );
        assert!(
            content.contains("explore"),
            "directory should list the explore type: {content}"
        );
    }

    /// A running child shows in the fleet list with its type and a running
    /// marker, taking precedence over the directory.
    #[test]
    fn test_fleet_row_precedes_directory() {
        let mut app = composition::app();
        app.agent_directory = Some("## Available agents".into());
        app.fleet.entries.push(FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "explore".into(),
            turn: 1,
            tokens: 50,
            tool_uses: 0,
            last_activity: None,
            completed: None,
            completed_at: None,
        });
        let content = render(&app, 40, 3);
        assert!(
            content.contains("explore"),
            "fleet row should show the child type: {content}"
        );
        assert!(
            content.contains("running"),
            "fleet row should show the running marker: {content}"
        );
    }

    /// An idle fleet with no fetched directory shows the placeholder, not a
    /// blank pane. Pins the no-session edge case the user hit.
    #[test]
    fn test_placeholder_when_no_directory() {
        let app = composition::app();
        let content = render(&app, 40, 3);
        assert!(
            content.contains("no agent directory"),
            "placeholder should render when the directory has not landed: {content}"
        );
    }
}
