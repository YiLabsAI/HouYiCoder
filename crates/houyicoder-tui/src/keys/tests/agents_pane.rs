//! Agents-pane key behavior: Esc closure, input retraction, and Enter
//! routing between the live fleet and the returned-delegation list. The
//! pane owns Esc while open (pane_owns_esc), retracts the input box like
//! the command panes, and routes Enter off the rendered list -- the live
//! fleet when it has entries, else the returned delegations -- so an
//! unselected fleet never opens an off-screen delegation.
use super::*;

/// Esc closes the /agents pane. The pane had no Esc branch at all -- the key
/// fell through to the queue-recall and abort branches, so while agents ran, Esc
/// interrupted the parent run instead of closing a pane, and idle it was a
/// dead key. The only way out was quitting.
#[test]
fn test_esc_closes_agents_pane() {
    let mut app = working_app();
    app.pane = Pane::Agents;
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.pane,
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
    app.pane = Pane::Agents;
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(app.pane, Pane::Transcript, "Esc closes the pane");
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
    let before = crate::test_harness::render_text(&app, 80, 24);
    assert!(
        before.contains("for commands"),
        "the input box shows its prompt while the transcript is up"
    );
    app.pane = Pane::Agents;
    let after = crate::test_harness::render_text(&app, 80, 24);
    assert!(
        !after.contains("for commands"),
        "the input box retracts while the agents pane is open:\n{after}"
    );
}

/// With a live fleet on screen, Enter acts on the fleet -- never on a
/// returned delegation the pane is not showing. The selection is None until
/// the user moves it, and keying the branch off the selection instead of off
/// the rendered list let that unselected case open an off-screen delegation.
#[test]
fn test_agents_enter_follows_list() {
    use crate::agent_message::FleetEntry;
    use crate::records::TranscriptLine;
    let mut app = working_app();
    app.pane = Pane::Agents;
    app.push_transcript_line(TranscriptLine::Subagent {
        child_sid: "returned".into(),
        subagent_type: "explore".into(),
        summary: "old".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("reply".into())],
        color: None,
    });
    app.sessions.current.refresh_agents();
    app.fleet.entries.push(FleetEntry {
        agent_id: "live".into(),
        subagent_type: "explore".into(),
        turn: 1,
        tokens: 10,
        tool_uses: 0,
        last_activity: None,
        completed: None,
        completed_at: None,
        started_at: None,
    });
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(
        app.teammate_view.is_none(),
        "an unselected live fleet opens nothing, not the off-screen delegation"
    );
    app.fleet.selected = Some(0);
    handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.teammate_view.as_ref().map(|v| v.child_sid.as_str()),
        Some("live"),
        "Enter opens the selected fleet child"
    );
}

/// With no live fleet, Enter drills into the selected returned delegation
/// and opens its view. Also drives the App-level accessors for the agents
/// pane state -- the public seam external integration tests reach the
/// session-owned rows through, since in-crate code reads the session field
/// directly.
#[test]
fn test_agents_enter_returned_delegation() {
    use crate::records::TranscriptLine;
    let mut app = working_app();
    app.pane = Pane::Agents;
    app.push_transcript_line(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("child reply".into())],
        color: None,
    });
    app.refresh_agents();
    assert_eq!(app.agents().rows.len(), 1, "accessor reads the rows");
    assert!(app.agents().rows[0].loaded, "loaded fold detected");
    app.agents_mut().move_selection(1);
    assert_eq!(app.agents().sel, 0, "cursor clamps at the single row");
    handle_working(&mut app, key(KeyCode::Enter));
    let view = app.teammate_view.as_ref().expect("view opened");
    assert_eq!(view.child_sid, "c1");
    assert!(
        !view.transcript.is_empty(),
        "the loaded fold is copied into the view"
    );
}
