//! The shared expand chord against the checklist and the child strip.
//!
//! Both cases are about which handler claims Ctrl+K or the expand chord
//! while the draft is empty, so they sit together rather than in the
//! general key file.

use super::*;
use crate::app::handle_key;
use crate::test_harness::{connected_app_with_events, wait_for_notification};

/// A connected working app holding one running child, selected in the strip.
/// The connection is real so a key that enqueues a command can be observed on
/// the wire; the child is real so the kill gesture has a target.
fn child_app() -> (App, std::sync::mpsc::Receiver<TransportEvent>) {
    let (mut app, events) = connected_app_with_events();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.viewport = ViewportMode::Working;
    app.sessions
        .current
        .capabilities
        .fleet
        .entries
        .push(FleetEntry {
            agent_id: "child-0".into(),
            subagent_type: "explore".into(),
            description: String::new(),
            turn: 1,
            tokens: 10,
            tool_uses: 0,
            last_activity: None,
            outcome: None,
            completed_at: None,
            started_at: None,
        });
    app.ui.fleet.focus(0);
    (app, events)
}

/// The checklist answers the shared expand chord. It used to sit behind the
/// tail fallback, which claims the chord whenever any foldable block is on
/// screen — in a real session that is always — so the checklist branch never
/// ran and the list could not be opened at all.
#[test]
fn test_ctrl_o_reaches() {
    let mut app = working_app();
    app.sessions.current.todos.items = vec![
        TodoView {
            content: "write the fix".into(),
            active_form: Some("writing the fix".into()),
            status: TodoStatus::Pending,
        },
        TodoView {
            content: "run the gate".into(),
            active_form: Some("running the gate".into()),
            status: TodoStatus::Pending,
        },
    ];
    // A foldable block at the tail, the state that used to swallow the chord.
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::ThoughtFor {
            ms: Some(9_000),
            reasoning: Some("a train of reasoning".into()),
            tool_summary: None,
            turn_id: "t1".into(),
        });
    assert!(!app.sessions.current.todos.expanded);
    handle_ctrl_o(&mut app);
    assert!(
        app.sessions.current.todos.expanded,
        "the chord reaches the checklist even with a fold at the tail"
    );
    handle_ctrl_o(&mut app);
    assert!(
        !app.sessions.current.todos.expanded,
        "and the same chord closes it again"
    );
}

/// The chord must work with the cursor on the checklist's own summary row.
/// That row publishes a fold key so a click can open the list, and the chord
/// path read the same key and handed it to the transcript, which stored a fold
/// group nothing owned: the key was consumed, the list stayed shut, and the
/// hint printed on the row was a lie.
#[test]
fn test_ctrl_o_opens() {
    let mut app = working_app();
    app.sessions.current.todos.items = (0..6)
        .map(|i| TodoView {
            content: format!("task {i}"),
            active_form: None,
            status: TodoStatus::Pending,
        })
        .collect();
    drop(render_text(&app, 80, 24));
    let summary = app
        .ui
        .frame
        .layout
        .last_transcript_rows
        .borrow()
        .iter()
        .position(|(_, text)| text.contains("hidden:"))
        .expect("the summary row is on screen");
    app.ui.selection.transcript.anchor = Some((0, summary));
    assert!(!app.sessions.current.todos.expanded);
    handle_ctrl_o(&mut app);
    assert!(
        app.sessions.current.todos.expanded,
        "the chord opens the checklist from its own summary row"
    );
}

/// A bare k on a selected running child stops it. This is the positive
/// control for the test below: the wire does carry a kill from this path, so
/// an absent kill there is the guard working rather than an unreachable
/// assertion.
#[test]
fn test_bare_k_kills() {
    let (mut app, events) = child_app();
    handle_key(&mut app, key(KeyCode::Char('k')));
    let params = wait_for_notification(&events, "session/kill_child");
    assert_eq!(
        params.get("childSid").and_then(|v| v.as_str()),
        Some("child-0"),
        "the selected child is the one named"
    );
}

/// A bare b on a selected running child hands it to the background, which is
/// the gesture that frees a turn the user is waiting on. The positive control
/// for the test below: the wire does carry the request from this path.
#[test]
fn test_bare_b_backgrounds() {
    let (mut app, events) = child_app();
    handle_key(&mut app, key(KeyCode::Char('b')));
    let params = wait_for_notification(&events, "session/background_child");
    assert_eq!(
        params.get("childSid").and_then(|v| v.as_str()),
        Some("child-0"),
        "the selected child is the one named"
    );
}

/// Ctrl+B belongs to the draft, not to the hand-over gesture. The hand-over
/// reads a bare b and runs only while the draft is empty, which is exactly
/// when a user presses Ctrl+B while typing; without the modifier guard the
/// chord would hand over a child nobody selected.
#[test]
fn test_ctrl_b_spares() {
    let (mut app, events) = child_app();
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
    );
    let window = std::time::Duration::from_millis(300);
    let deadline = std::time::Instant::now() + window;
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        match events.recv_timeout(remaining) {
            Ok(TransportEvent::Frame(frame)) => assert!(
                !frame.contains("background_child"),
                "Ctrl+B edits the draft, it does not hand a child over: {frame}"
            ),
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

/// Ctrl+K belongs to the draft, not to the kill-child gesture. The kill
/// gesture reads a bare k and runs only while the draft is empty, which is
/// exactly when a user presses Ctrl+K to throw the draft away; without the
/// modifier guard the chord would stop a running child nobody selected.
#[test]
fn test_ctrl_k_spares() {
    let (mut app, events) = child_app();
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
    );
    let window = std::time::Duration::from_millis(300);
    let deadline = std::time::Instant::now() + window;
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        match events.recv_timeout(remaining) {
            Ok(TransportEvent::Frame(frame)) => assert!(
                !frame.contains("kill_child"),
                "Ctrl+K edits the draft, it does not stop a child: {frame}"
            ),
            Ok(_) => {}
            Err(_) => break,
        }
    }
}
