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
pub(super) fn draw_agents(f: &mut Frame, area: Rect, app: &App) {
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
        f.render_widget(List::new(items).style(Style::new().fg(Color::White)), area);
        return;
    }
    // Returned delegations outlive the footer strip: the strip retires a
    // completed child after its grace window, but the pane is the record of
    // what ran this session, selectable and enterable.
    if !app.agents.rows.is_empty() {
        let items: Vec<ListItem> = app
            .agents
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let row = format!("\u{2713} {} \u{00b7} {}", r.subagent_type, r.summary);
                if app.agents.sel == i {
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

    /// Once delegations have returned, the pane lists them with a cursor
    /// marker instead of the registered-agent directory: the directory is
    /// what exists, the list is what ran, and what ran is the actionable
    /// thing. The marker shows which row Enter opens.
    #[test]
    fn test_returned_list_over_directory() {
        use crate::records::TranscriptLine;
        let mut app = composition::app();
        app.agent_directory = Some("## Available agents\n- explore: fast".into());
        app.push_transcript_line(TranscriptLine::Subagent {
            child_sid: "c1".into(),
            subagent_type: "explore".into(),
            summary: "found the auth path".into(),
            prompt: String::new(),
            folded_transcript: Vec::new(),
            color: None,
        });
        let v = app.transcript_version.get();
        app.agents.refresh(&app.transcript, v);
        let out = render(&app, 60, 8);
        assert!(
            out.contains("found the auth path"),
            "the returned delegation is listed: {out}"
        );
        assert!(
            !out.contains("Available agents"),
            "the directory yields to the session list: {out}"
        );
        assert!(
            out.contains("\u{25b6}"),
            "the selected row carries the cursor marker: {out}"
        );
    }
}
