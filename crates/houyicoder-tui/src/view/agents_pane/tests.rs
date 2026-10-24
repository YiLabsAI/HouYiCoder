use super::*;
use crate::agent_message::FleetEntry;
use crate::composition;
use crate::records::TranscriptLine;
use houyicoder_protocol::frontend::agents::AgentPrompt;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::time::{Duration, Instant};

fn render(app: &App, w: u16, h: u16) -> String {
    render_lines(app, w, h).join("")
}

/// The pane's rows as text, so a test can ask which column a cell starts at
/// and not only whether a string is somewhere on screen.
fn render_lines(app: &App, w: u16, h: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal
        .draw(|f| {
            draw_agents(f, f.area(), app);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..h)
        .map(|y| {
            (0..w)
                .map(|x| {
                    buffer
                        .cell((x, y))
                        .map(|cell| cell.symbol())
                        .unwrap_or(" ")
                        .to_string()
                })
                .collect::<String>()
        })
        .collect()
}

/// The display column a cell starts at in one rendered row. Counts columns
/// rather than bytes, so the cursor glyph in front of a row cannot read as a
/// longer row than the one without it.
fn column_of(line: &str, cell: &str) -> Option<usize> {
    line.find(cell)
        .map(|at| UnicodeWidthStr::width(&line[..at]))
}

fn entry(subagent_type: &str, when_to_use: &str) -> AgentEntry {
    AgentEntry {
        subagent_type: subagent_type.into(),
        when_to_use: when_to_use.into(),
        tools: Some(vec!["Read".into(), "Grep".into()]),
        disallowed_tools: vec!["Write".into()],
        model: Some("sonnet".into()),
        effort: None,
        system_prompt: AgentPrompt::Owned("you search the tree".into()),
    }
}

fn install_types(app: &mut App, types: Vec<AgentEntry>) {
    app.sessions.current.capabilities.agent_types.entries = Some(types);
}

fn push_running(app: &mut App, agent_id: &str) {
    app.sessions
        .current
        .capabilities
        .fleet
        .entries
        .push(FleetEntry {
            agent_id: agent_id.into(),
            subagent_type: "explore".into(),
            turn: 2,
            tokens: 1400,
            tool_uses: 0,
            last_activity: None,
            completed: None,
            completed_at: None,
            started_at: None,
        });
}

fn push_returned(app: &mut App, child_sid: &str, summary: &str) {
    app.push_transcript_line(TranscriptLine::Subagent {
        child_sid: child_sid.into(),
        subagent_type: "explore".into(),
        summary: summary.into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    app.sessions.current.refresh_agents();
}

/// One screen answers what has run and what may be delegated to: the running
/// children, the returned delegations, and the registered types each carry
/// their own heading, and the count line reports each source.
#[test]
fn test_list_renders_three_sections() {
    let mut app = composition::app();
    install_types(
        &mut app,
        vec![entry("general-purpose", "for anything else")],
    );
    push_running(&mut app, "c1");
    push_returned(&mut app, "c0", "found the auth path");
    let out = render(&app, 60, 16);
    for heading in ["Running (1)", "Completed (1)", "Available types (1)"] {
        assert!(out.contains(heading), "{heading} should render: {out}");
    }
    assert!(out.contains("found the auth path"), "the row body: {out}");
    assert!(
        out.contains("1.4k"),
        "the running row counts its tokens: {out}"
    );
    assert!(
        out.contains("1 types \u{00b7} 1 running \u{00b7} 1 completed"),
        "the count line reports every source: {out}"
    );
}

/// The list holds the newest delegations, so the count line names the older
/// ones it holds back rather than reporting the listed rows as the session's
/// total.
#[test]
fn test_count_names_withheld_older() {
    let mut app = composition::app();
    for index in 0..12 {
        push_returned(&mut app, &format!("c{index}"), "done");
    }
    let out = render(&app, 60, 20);
    assert!(
        out.contains("10 completed (2 older)"),
        "the count names the rows the list holds back: {out}"
    );
}

/// A reply that has not landed and a registry that registered nothing are
/// different facts, so they do not share one placeholder.
#[test]
fn test_empty_pane_two_placeholders() {
    let app = composition::app();
    let out = render(&app, 60, 10);
    assert!(
        out.contains("(no agent types loaded)"),
        "an unfetched list says so: {out}"
    );
    let mut app = composition::app();
    install_types(&mut app, Vec::new());
    let out = render(&app, 60, 10);
    assert!(
        out.contains("(no agent types registered)"),
        "a fetched but empty list says so: {out}"
    );
}

/// The cursor marker lands on the row the pane cursor names and nowhere else,
/// so the highlighted row is the row Enter reaches.
#[test]
fn test_marker_follows_cursor() {
    let mut app = composition::app();
    install_types(
        &mut app,
        vec![entry("explore", "fast search"), entry("planner", "slow")],
    );
    let out = render(&app, 60, 12);
    assert!(
        out.contains("\u{25b6} - explore"),
        "the first row carries the marker: {out}"
    );
    assert!(
        !out.contains("\u{25b6} - planner"),
        "the other row carries none: {out}"
    );
    app.ui.agents_pane.move_cursor(1, 2);
    let out = render(&app, 60, 12);
    assert!(
        out.contains("\u{25b6} - planner"),
        "the marker follows the cursor: {out}"
    );
}

/// A type's detail shows the definition the pane received: when to use it,
/// the tools it runs with, and its system prompt.
#[test]
fn test_detail_renders_fields() {
    let mut app = composition::app();
    install_types(&mut app, vec![entry("explore", "fast search")]);
    app.ui.agents_pane.open_detail("explore".into());
    let out = render(&app, 60, 16);
    assert!(out.contains("When to use: fast search"), "{out}");
    assert!(out.contains("Tools: Read, Grep"), "{out}");
    assert!(out.contains("Disallowed: Write"), "{out}");
    assert!(out.contains("System prompt"), "{out}");
    assert!(out.contains("you search the tree"), "{out}");
    assert!(
        !out.contains("Available types"),
        "the list is behind the detail: {out}"
    );
}

/// A definition that names no tools takes the caller's, and one with no
/// owned prompt takes the caller's prompt: an unset field says it inherits
/// rather than printing nothing.
#[test]
fn test_detail_names_inherited_fields() {
    let mut app = composition::app();
    install_types(
        &mut app,
        vec![AgentEntry {
            tools: None,
            model: None,
            system_prompt: AgentPrompt::InheritParent,
            ..entry("explore", "fast search")
        }],
    );
    app.ui.agents_pane.open_detail("explore".into());
    let out = render(&app, 60, 16);
    assert!(out.contains("inherits the caller's tools"), "{out}");
    assert!(out.contains("inherits the caller's prompt"), "{out}");
}

/// A type dropped by a refresh leaves no stale copy on screen: the detail
/// reads the capability at render time, so it reports that the type is gone.
#[test]
fn test_detail_reports_dropped_type() {
    let mut app = composition::app();
    install_types(&mut app, vec![entry("explore", "fast search")]);
    app.ui.agents_pane.open_detail("explore".into());
    install_types(&mut app, Vec::new());
    let out = render(&app, 60, 16);
    assert!(out.contains("(type no longer registered)"), "{out}");
    assert!(
        !out.contains("you search the tree"),
        "no stale prompt body survives the drop: {out}"
    );
}

/// A list longer than the pane draws follows the cursor: the selected row is on
/// screen whichever row it is, and the rows it scrolled past are not, so a key
/// always reaches the row the user sees highlighted.
#[test]
fn test_list_follows_cursor() {
    let mut app = composition::app();
    let types: Vec<AgentEntry> = (0..15)
        .map(|index| entry(&format!("type{index:02}"), "when it helps"))
        .collect();
    install_types(&mut app, types);
    let out = render(&app, 60, 12);
    assert!(
        out.contains("\u{25b6} - type00"),
        "the list opens on its first row: {out}"
    );
    app.ui.agents_pane.move_cursor(14, 15);
    let out = render(&app, 60, 12);
    assert!(
        out.contains("\u{25b6} - type14"),
        "the selected row is drawn: {out}"
    );
    assert!(
        !out.contains("- type00"),
        "the rows above the window are not: {out}"
    );
}

/// A long type name does not shift the cells behind it: every available row
/// starts its when-to-use at one column, so the section reads as a table.
#[test]
fn test_long_type_no_shift() {
    let mut app = composition::app();
    install_types(
        &mut app,
        vec![
            entry("general-purpose", "for anything"),
            entry("explore", "fast search"),
        ],
    );
    let lines = render_lines(&app, 80, 16);
    let wide = lines
        .iter()
        .find_map(|line| column_of(line, "for anything"));
    let short = lines.iter().find_map(|line| column_of(line, "fast search"));
    assert_eq!(
        wide,
        short,
        "the description column is the pane's, not the row's:\n{}",
        lines.join("\n")
    );
}

/// A short when-to-use holds its budget, so the cost cell after it starts at
/// one column down the list rather than stepping with the description length.
#[test]
fn test_short_description_no_shift() {
    let mut app = composition::app();
    install_types(
        &mut app,
        vec![
            entry("general-purpose", "for anything"),
            entry("explore", "fast"),
        ],
    );
    let lines = render_lines(&app, 80, 16);
    let cells: Vec<usize> = lines
        .iter()
        .filter_map(|line| column_of(line, " \u{00b7} sonnet"))
        .collect();
    assert_eq!(
        cells.len(),
        2,
        "both rows carry a cost cell:\n{}",
        lines.join("\n")
    );
    assert_eq!(
        cells[0],
        cells[1],
        "the cost cell starts at one column:\n{}",
        lines.join("\n")
    );
}

#[test]
fn test_completed_metrics_render() {
    let mut app = composition::app();
    let start = Instant::now() - Duration::from_secs(125);
    let end = Instant::now();
    app.sessions
        .current
        .capabilities
        .fleet
        .entries
        .push(FleetEntry {
            agent_id: "c-metrics".into(),
            subagent_type: "verify".into(),
            turn: 3,
            tokens: 24_500,
            tool_uses: 18,
            last_activity: None,
            completed: Some("all checks green".into()),
            completed_at: Some(end),
            started_at: Some(start),
        });
    app.push_transcript_line(TranscriptLine::Subagent {
        child_sid: "c-metrics".into(),
        subagent_type: "verify".into(),
        summary: "all checks green".into(),
        prompt: "run suite".into(),
        folded_transcript: Vec::new(),
        color: None,
    });
    app.sessions.current.refresh_agents();
    let list_out = render(&app, 80, 14);
    assert!(
        list_out.contains("18 calls \u{00b7} 24.5k tok \u{00b7} 2m 5s"),
        "completed L0 row shows calls, tokens, and duration:\n{list_out}"
    );
    app.ui
        .agents_pane
        .open_child_detail("verify".into(), "c-metrics".into());
    let detail_out = render(&app, 80, 14);
    assert!(
        detail_out.contains("Metrics: 18 calls \u{00b7} 24.5k tok \u{00b7} 2m 5s"),
        "completed L1 detail shows metrics:\n{detail_out}"
    );
    assert!(detail_out.contains("Prompt: run suite"), "{detail_out}");
}
