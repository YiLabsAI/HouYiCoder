//! Key routing for the /sandbox pane: what each screen's keys do, and the
//! letters the editor keeps while the user is writing a message.

use std::sync::mpsc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::checkpoint::{
    CheckpointImage, CheckpointListView, CheckpointState, CheckpointView,
};

use crate::composition;
use crate::keys::handle_working;
use crate::state::{App, Pane};
use crate::test_harness::{TransportEvent, connected_app_with_events, render_text};
use crate::ui::{CheckpointLevel, RestoreFocus, SandboxTab};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn image(present: bool, hash: Option<&str>) -> CheckpointImage {
    CheckpointImage {
        present,
        hash: hash.map(str::to_string),
        bytes: hash.map(|_| 12),
    }
}

fn write(operation: &str) -> CheckpointView {
    CheckpointView {
        operation: operation.into(),
        target: format!("src/{operation}.rs"),
        state: CheckpointState::Applied,
        written_ms: Some(1_700_000_000_000),
        before: image(true, Some("before")),
        after: image(true, Some("after")),
    }
}

/// An App with a connection, the pane open on the checkpoints tab with two
/// rows, and an empty input box.
fn sandbox_app() -> (App, mpsc::Receiver<TransportEvent>) {
    let (mut app, events) = connected_app_with_events();
    app.ui.navigation.pane = Pane::Sandbox;
    app.ui.sandbox_pane.set_tab(SandboxTab::Checkpoints);
    let request = RequestId(71);
    app.ui.sandbox_pane.expect_list(request);
    app.apply_sandbox_checkpoints(
        request,
        CheckpointListView {
            checkpoints: vec![write("cp-1"), write("cp-2")],
            unreadable: Vec::new(),
        },
    );
    app.ui.input.editor.clear();
    (app, events)
}

#[test]
fn test_down_moves_cursor() {
    let (mut app, _events) = sandbox_app();
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(
        app.ui.sandbox_pane.cursor.get(),
        1,
        "Down selects the next row"
    );
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(app.ui.sandbox_pane.cursor.get(), 0, "Up returns one row");
}

#[test]
fn test_enter_opens_detail() {
    let (mut app, _events) = sandbox_app();
    handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(app.ui.sandbox_pane.level.get(), CheckpointLevel::Detail);
}

#[test]
fn test_arrows_switch_tab() {
    let (mut app, _events) = sandbox_app();
    handle_working(&mut app, key(KeyCode::Left));
    assert_eq!(
        app.ui.sandbox_pane.tab.get(),
        SandboxTab::Boundaries,
        "Left reaches the other tab"
    );
    handle_working(&mut app, key(KeyCode::Right));
    assert_eq!(app.ui.sandbox_pane.tab.get(), SandboxTab::Checkpoints);
}

/// Tab reaches the other tab, as the tab bar says. The key belongs to the
/// pane here, so it must not be read as the global pane cycle.
#[test]
fn test_tab_switches_tabs() {
    let (mut app, _events) = sandbox_app();
    handle_working(&mut app, key(KeyCode::Tab));
    assert_eq!(
        app.ui.sandbox_pane.tab.get(),
        SandboxTab::Boundaries,
        "Tab reaches the other tab"
    );
    handle_working(&mut app, key(KeyCode::Tab));
    assert_eq!(app.ui.sandbox_pane.tab.get(), SandboxTab::Checkpoints);
}

/// Without a connection the global Tab moves between panes. The sandbox pane
/// is open on purpose, so the key stays inside it.
#[test]
fn test_tab_stays_in_pane() {
    let mut app = composition::app();
    app.ui.navigation.pane = Pane::Sandbox;
    handle_working(&mut app, key(KeyCode::Tab));
    assert_eq!(app.ui.navigation.pane, Pane::Sandbox);
    assert_eq!(app.ui.sandbox_pane.tab.get(), SandboxTab::Checkpoints);
}

/// On the confirmation Tab moves the buttons' focus, which the bracketed
/// button shows.
#[test]
fn test_tab_moves_focus() {
    let (mut app, _events) = sandbox_app();
    handle_working(&mut app, key(KeyCode::Char('r')));
    handle_working(&mut app, key(KeyCode::Tab));
    assert_eq!(app.ui.sandbox_pane.focus.get(), RestoreFocus::Restore);
    let text = render_text(&app, 100, 30);
    assert!(text.contains("[Restore checkpoint]"), "{text}");
    assert!(!text.contains("[Cancel]"), "{text}");
}

#[test]
fn test_r_opens_restore() {
    let (mut app, _events) = sandbox_app();
    handle_working(&mut app, key(KeyCode::Char('r')));
    assert_eq!(app.ui.sandbox_pane.level.get(), CheckpointLevel::Restore);
    assert_eq!(
        app.ui.sandbox_pane.focus.get(),
        RestoreFocus::Cancel,
        "the confirmation starts on the button that changes nothing"
    );
}

#[test]
fn test_enter_cancels_by_default() {
    let (mut app, _events) = sandbox_app();
    handle_working(&mut app, key(KeyCode::Char('r')));
    handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.ui.sandbox_pane.level.get(),
        CheckpointLevel::List,
        "Enter on the default focus backs out without restoring"
    );
}

#[test]
fn test_typing_keeps_letters() {
    let (mut app, _events) = sandbox_app();
    app.ui.input.editor.set("note to self".to_string());
    handle_working(&mut app, key(KeyCode::Char('r')));
    assert_eq!(
        app.ui.sandbox_pane.level.get(),
        CheckpointLevel::List,
        "a letter typed into the message does not open the confirmation"
    );
}

/// The boundaries screen names the place a refused write is granted, and the
/// key that goes there.
#[test]
fn test_p_opens_permissions() {
    let (mut app, _events) = sandbox_app();
    app.ui.sandbox_pane.set_tab(SandboxTab::Boundaries);
    handle_working(&mut app, key(KeyCode::Char('p')));
    assert_eq!(app.ui.navigation.pane, Pane::Permission);
}

#[test]
fn test_esc_closes_at_list() {
    let (mut app, _events) = sandbox_app();
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(app.ui.navigation.pane, Pane::Transcript);
}

/// Below the listing Esc returns one level and keeps the pane open, so the
/// key closes the pane only at its root.
#[test]
fn test_esc_returns_from_detail() {
    let (mut app, _events) = sandbox_app();
    handle_working(&mut app, key(KeyCode::Enter));
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(app.ui.sandbox_pane.level.get(), CheckpointLevel::List);
    assert_eq!(app.ui.navigation.pane, Pane::Sandbox);
}
