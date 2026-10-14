use super::*;
use crate::agent_message::FleetEntry;
use crate::composition;
use crate::records::TranscriptLine;
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
    app.sessions.current.agent_directory = Some("## Available agents\n\n- explore: fast".into());
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
    app.sessions.current.agent_directory = Some("## Available agents".into());
    app.sessions
        .current
        .capabilities
        .fleet
        .entries
        .push(FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "explore".into(),
            turn: 1,
            tokens: 50,
            tool_uses: 0,
            last_activity: None,
            completed: None,
            completed_at: None,
            started_at: None,
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
    let mut app = composition::app();
    app.sessions.current.agent_directory = Some("## Available agents\n- explore: fast".into());
    app.push_transcript_line(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found the auth path".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    app.sessions.current.refresh_agents();
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

/// The cursor marker lands on the row the Ui-owned selection names, and
/// a selection past the end still marks the last row: the render clamps
/// the cursor against the session's rows.
#[test]
fn test_marker_follows_selection() {
    let mut app = composition::app();
    for (sid, summary) in [("c1", "first row"), ("c2", "second row")] {
        app.push_transcript_line(TranscriptLine::Subagent {
            child_sid: sid.into(),
            subagent_type: "explore".into(),
            summary: summary.into(),
            prompt: String::new(),
            folded_transcript: Vec::new(),
            color: None,
        });
    }
    app.sessions.current.refresh_agents();
    let marked = "\u{25b6} \u{2713} explore \u{00b7} ";
    app.ui.agents_sel = 1;
    let out = render(&app, 60, 8);
    assert!(
        out.contains(&format!("{marked}second row")),
        "the marker follows the cursor: {out}"
    );
    assert!(
        !out.contains(&format!("{marked}first row")),
        "the unselected row carries no marker: {out}"
    );
    app.ui.agents_sel = 5;
    let out = render(&app, 60, 8);
    assert!(
        out.contains(&format!("{marked}second row")),
        "a stale cursor clamps to the last row: {out}"
    );
}
