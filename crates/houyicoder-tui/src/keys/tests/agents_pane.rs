//! Agents-pane key behavior: Esc closure, input retraction, the list arms,
//! and the detail the pane opens over the list. One cursor walks every
//! rendered row, so the row the keys reach is the row the pane highlights,
//! and the detail answers the same scroll keys as the /skills detail.
use super::*;
use crate::agent_message::FleetEntry;
use crate::records::TranscriptLine;
use crate::test_harness::render_text;
use houyicoder_protocol::frontend::agents::{AgentEntry, AgentPrompt};

fn push_running(app: &mut App, agent_id: &str) {
    app.sessions
        .current
        .capabilities
        .fleet
        .entries
        .push(FleetEntry {
            agent_id: agent_id.into(),
            subagent_type: "explore".into(),
            turn: 1,
            tokens: 10,
            tool_uses: 0,
            last_activity: None,
            completed: None,
            completed_at: None,
            started_at: None,
        });
}

fn push_returned(app: &mut App, child_sid: &str, folded: bool) {
    let folded_transcript = if folded {
        vec![TranscriptLine::Agent("child reply".into())]
    } else {
        Vec::new()
    };
    app.push_transcript_line(TranscriptLine::Subagent {
        child_sid: child_sid.into(),
        subagent_type: "explore".into(),
        summary: format!("summary of {child_sid}"),
        prompt: String::new(),
        folded_transcript,
        color: None,
    });
    app.refresh_agents();
}

fn install_type(app: &mut App, subagent_type: &str) {
    app.sessions.current.capabilities.agent_types.entries = Some(vec![AgentEntry {
        subagent_type: subagent_type.into(),
        when_to_use: "when it helps".into(),
        tools: None,
        disallowed_tools: Vec::new(),
        model: None,
        effort: None,
        system_prompt: AgentPrompt::InheritParent,
    }]);
}

/// Esc closes the /agents pane. The pane had no Esc branch at all -- the key
/// fell through to the queue-recall and abort branches, so while agents ran, Esc
/// interrupted the parent run instead of closing a pane, and idle it was a
/// dead key. The only way out was quitting.
#[test]
fn test_esc_closes_agents_pane() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Agents;
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Transcript,
        "Esc must close the agents pane back to the transcript"
    );
}

/// While agents run, Esc on the open pane closes the pane -- it must not
/// reach the abort branch. pane_owns_esc gates that; the agents pane was not on
/// the list.
#[test]
fn test_agents_esc_no_abort() {
    let mut app = working_app();
    app.start_run_for_test(0);
    app.ui.navigation.pane = Pane::Agents;
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Transcript,
        "Esc closes the pane"
    );
    assert!(
        !app.cancelling(),
        "a pane-open Esc must not interrupt the running agents"
    );
}

/// The agents pane retracts the input box like the other command panes.
/// It sat on neither of the two pane lists, so the input box stayed up while
/// the pane was open -- inconsistent with every other pane and misleading,
/// since typing there does not reach the pane.
#[test]
fn test_agents_pane_hides_input() {
    let mut app = working_app();
    let before = render_text(&app, 80, 24);
    assert!(
        before.contains("for commands"),
        "the input box shows its prompt while the transcript is up"
    );
    app.ui.navigation.pane = Pane::Agents;
    let after = render_text(&app, 80, 24);
    assert!(
        !after.contains("for commands"),
        "the input box retracts while the agents pane is open:\n{after}"
    );
}

/// Enter acts on the row the pane highlights. The rows are listed in one
/// order, so a live child above a returned delegation means the first Enter
/// opens the child and the next one, after an arrow, opens the delegation.
/// Keying off the fleet's own selection instead opened a row the pane was
/// not showing.
#[test]
fn test_enter_opens_highlighted_row() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Agents;
    push_returned(&mut app, "returned", true);
    push_running(&mut app, "live");
    handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.ui.teammate_view.as_ref().map(|v| v.child_sid.as_str()),
        Some("live"),
        "the running child leads the list and Enter opens it"
    );
    handle_working(&mut app, key(KeyCode::Down));
    handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.ui.teammate_view.as_ref().map(|v| v.child_sid.as_str()),
        Some("returned"),
        "the next row is the returned delegation"
    );
}

/// A registered type opens its detail instead of a child view: nothing ran,
/// so there is no transcript to drill into.
#[test]
fn test_enter_opens_type_detail() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Agents;
    install_type(&mut app, "explore");
    handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.ui
            .agents_pane
            .detail()
            .map(|detail| detail.subagent_type()),
        Some("explore"),
        "Enter opens the type's detail"
    );
    assert!(
        app.ui.teammate_view.is_none(),
        "a type has no child view to open"
    );
}

/// Esc returns one level: the detail closes back to the list, and only the
/// list closes the pane. Closing the pane on the first Esc would drop the
/// user out of the pane they were reading.
#[test]
fn test_esc_returns_to_list() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Agents;
    install_type(&mut app, "explore");
    app.ui.agents_pane.open_detail("explore".into());
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(app.ui.agents_pane.detail().is_none(), "the detail closes");
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Agents,
        "the pane stays open on the list behind it"
    );
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(app.ui.navigation.pane, Pane::Transcript);
}

/// While the detail is open the arrows scroll it and the cursor stays put:
/// the same keys reach the pane that is on top.
#[test]
fn test_detail_arrows_scroll() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Agents;
    install_type(&mut app, "explore");
    app.ui.agents_pane.open_detail("explore".into());
    app.ui.agents_pane.set_detail_max_offset(50);
    handle_working(&mut app, key(KeyCode::PageDown));
    assert_eq!(app.ui.agents_pane.detail_offset(), 10);
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(app.ui.agents_pane.detail_offset(), 11);
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(app.ui.agents_pane.detail_offset(), 10);
    handle_working(&mut app, key(KeyCode::PageUp));
    assert_eq!(app.ui.agents_pane.detail_offset(), 0);
    assert_eq!(
        app.ui.agents_pane.cursor(),
        0,
        "the list cursor does not move behind the detail"
    );
}

/// The pane cursor and the footer strip name the same child: landing on a
/// running row writes the strip's selection, and landing on any other row
/// leaves the strip's own choice alone rather than clearing it.
#[test]
fn test_cursor_mirrors_running_row() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Agents;
    push_returned(&mut app, "returned", true);
    push_running(&mut app, "live");
    install_type(&mut app, "explore");
    assert!(app.ui.fleet.selected.is_none(), "no strip choice yet");
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(
        app.ui.fleet.selected,
        Some(0),
        "the running row writes the strip's selection"
    );
    app.ui.fleet.selected = Some(2);
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(app.ui.agents_pane.cursor(), 1, "the cursor reached row two");
    assert_eq!(
        app.ui.fleet.selected,
        Some(2),
        "a row with no child leaves the strip's own choice alone"
    );
}

/// Opening the pane settles the cursor on the first row, so a cursor standing
/// on a running child seeds the strip's selection then: the two highlights
/// agree before any arrow is pressed.
#[test]
fn test_open_seeds_strip_selection() {
    let mut app = working_app();
    push_running(&mut app, "live");
    app.ui.navigation.pane = Pane::Agents;
    assert!(app.ui.fleet.selected.is_none(), "no strip choice yet");
    app.refresh_agents();
    assert_eq!(
        app.ui.fleet.selected,
        Some(0),
        "the running row the pane opens on writes the strip's selection"
    );
}

/// A selection the user made in the footer strip stands when the pane opens:
/// the pane seeds only a strip that has no choice of its own.
#[test]
fn test_open_keeps_strip_choice() {
    let mut app = working_app();
    push_running(&mut app, "a");
    push_running(&mut app, "b");
    app.ui.navigation.pane = Pane::Agents;
    app.ui.fleet.selected = Some(1);
    app.refresh_agents();
    assert_eq!(
        app.ui.fleet.selected,
        Some(1),
        "the strip's own choice is not overwritten"
    );
}

/// The arrows step the cursor with clamping, Enter opens the highlighted row,
/// and a cursor past the end still opens the last row -- the Enter drill
/// clamps against the rows exactly as the marker does.
#[test]
fn test_cursor_clamps_both_ends() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Agents;
    push_returned(&mut app, "c1", true);
    push_returned(&mut app, "c2", true);
    handle_working(&mut app, key(KeyCode::Up));
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(app.ui.agents_pane.cursor(), 0, "the first row is the top");
    handle_working(&mut app, key(KeyCode::Down));
    handle_working(&mut app, key(KeyCode::Down));
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(app.ui.agents_pane.cursor(), 1, "the last row is the bottom");
    handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.ui.teammate_view.as_ref().map(|v| v.child_sid.as_str()),
        Some("c1"),
        "the cursor stopped at the last row, so Enter opens that one"
    );
}

/// A rebuild that empties the list parks the cursor at the top, so a later
/// repopulation starts from the first row instead of inheriting a position
/// from the previous list.
#[test]
fn test_rebuild_parks_cursor() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Agents;
    push_returned(&mut app, "c1", false);
    push_returned(&mut app, "c2", false);
    app.move_agents_cursor(1);
    assert_eq!(app.ui.agents_pane.cursor(), 1);
    app.sessions.current.transcript = Default::default();
    app.refresh_agents();
    assert_eq!(app.agents().rows.len(), 0, "the rebuild emptied the rows");
    assert_eq!(app.ui.agents_pane.cursor(), 0, "the emptied list parks it");
}

/// A returned delegation whose fold was never loaded opens its view with the
/// on-demand fetch, and its view is empty until the fetch answers -- the /
/// agents list must not claim a transcript it does not have.
#[test]
fn test_enter_fetches_unloaded_child() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Agents;
    push_returned(&mut app, "c1", false);
    assert!(!app.agents().rows[0].loaded, "the fold was never loaded");
    handle_working(&mut app, key(KeyCode::Enter));
    let view = app.ui.teammate_view.as_ref().expect("view opened");
    assert_eq!(view.child_sid, "c1");
    assert!(
        view.transcript.is_empty(),
        "an unloaded fold leaves the view to be filled by the fetch"
    );
}

/// The pane on screen keeps the viewport: the paging keys are inert while one
/// is open, and with none open the same key still enters the scroll viewport.
/// The predicate behind this is shared by every pane in the family, and the
/// binary-level journey pins the real key bytes.
#[test]
fn test_pane_keeps_screen() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Agents;
    install_type(&mut app, "explore");
    for index in 0..40 {
        app.push_transcript_line(TranscriptLine::Agent(format!("line {index}")));
    }
    render_text(&app, 80, 24);
    app.scroll_transcript_up();
    let back = app.whole_scroll().top_offset();
    assert!(back > 0, "the transcript is scrolled back from the tail");
    handle_working(&mut app, key(KeyCode::PageUp));
    handle_working(&mut app, key(KeyCode::PageDown));
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("explore"),
        "the pane is still the screen the keys reached:\n{out}"
    );
    assert!(
        !out.contains("SCROLL"),
        "paging under a pane does not enter the scroll viewport:\n{out}"
    );
    assert_eq!(
        app.whole_scroll().top_offset(),
        back,
        "and does not page the transcript behind the pane"
    );
    app.ui.navigation.pane = Pane::Transcript;
    handle_working(&mut app, key(KeyCode::PageDown));
    assert!(
        app.whole_scroll().top_offset() > back,
        "with no pane open page-down pages the transcript"
    );
    handle_working(&mut app, key(KeyCode::PageUp));
    assert!(
        render_text(&app, 80, 24).contains("SCROLL"),
        "with no pane open page-up enters the scroll viewport"
    );
    // Focus is a second dispatch entry for the same key: it draws the pane
    // full screen and routes through its own table, so the pane must keep
    // that viewport too.
    app.ui.navigation.pane = Pane::Agents;
    app.ui.navigation.viewport = ViewportMode::Focus;
    handle_working(&mut app, key(KeyCode::PageUp));
    assert_eq!(
        app.ui.navigation.viewport,
        ViewportMode::Focus,
        "page-up in the focus viewport leaves the pane's own screen standing"
    );
}
