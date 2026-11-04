//! DirectShell key routing: the ghost suffix, submission, and the two ways
//! out of the mode.

use super::super::handle_input;
use crate::composition;
use crate::input::InputMode;
use crate::state::{App, Screen};
use crate::test_harness::{connected_app_events, wait_for_request};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::frontend::FrontendRequest;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// An app on the working screen with the transcript pane focused.
fn working_app() -> App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app
}

/// A working app with the composer in DirectShell mode holding a ghost suffix.
fn shell_app_with_ghost(draft: &str, suffix: &str) -> App {
    let mut app = working_app();
    app.ui.input.mode = InputMode::DirectShell;
    app.ui.input.editor.set(draft.to_string());
    app.ui.input.ghost = Some(suffix.to_string());
    app
}

/// Tab accepts the ghost suffix into the editor without running the command.
#[test]
fn test_tab_accepts_ghost() {
    let mut app = shell_app_with_ghost("cargo t", "est");
    handle_input(&mut app, key(KeyCode::Tab));
    assert_eq!(app.ui.input.editor.value(), "cargo test");
    assert!(app.ui.input.ghost.is_none());
    assert_eq!(app.ui.input.mode, InputMode::DirectShell);
}

/// A control chord pressed here inserts nothing. This router has its own
/// character case, separate from the chat one, so a guard that exists only
/// there would leave a shortcut the product does not bind writing its own
/// character into a shell command.
#[test]
fn test_ctrl_chord_ignored() {
    let mut app = shell_app_with_ghost("cargo t", "est");
    for code in ['v', 'y', 'b', 'q'] {
        handle_input(
            &mut app,
            KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL),
        );
    }
    assert_eq!(
        app.ui.input.editor.value(),
        "cargo t",
        "an unbound chord leaves the shell command as it was"
    );
}

/// Enter sends the typed command (never the ghost), clears the editor, and
/// returns the composer to Chat once the host took it.
#[test]
fn test_enter_runs_typed() {
    let (mut app, events) = connected_app_events();
    app.ui.input.mode = InputMode::DirectShell;
    app.ui.input.editor.set("cargo t".into());
    app.ui.input.ghost = Some("est".into());
    handle_input(&mut app, key(KeyCode::Enter));
    assert_eq!(app.ui.input.mode, InputMode::Chat);
    assert!(
        app.ui.input.editor.is_empty(),
        "the editor clears after the host took the command"
    );
    wait_for_request(
        &events,
        |payload| matches!(payload, FrontendRequest::DirectShellRun { command, .. } if command == "cargo t"),
    );
    assert!(
        app.sessions.current.shell.request_id().is_some(),
        "the submitted command is the one running"
    );
}

/// A command the host cannot take leaves the composer exactly as the user
/// typed it, in shell mode, so retrying after the blocking work finishes costs
/// no retyping.
#[test]
fn test_enter_keeps_refused() {
    let mut app = shell_app_with_ghost("cargo t", "est");
    handle_input(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.ui.input.mode,
        InputMode::DirectShell,
        "a refused command stays in the shell composer"
    );
    assert_eq!(
        app.ui.input.editor.value(),
        "cargo t",
        "the typed command survives the refusal"
    );
    assert!(
        !app.sessions.current.shell.is_active(),
        "nothing runs after a refusal"
    );
}

/// Esc drops the ghost first and stays in the mode; a second Esc exits.
#[test]
fn test_esc_clears_then_exits() {
    let mut app = shell_app_with_ghost("cargo t", "est");
    handle_input(&mut app, key(KeyCode::Esc));
    assert!(app.ui.input.ghost.is_none());
    assert_eq!(app.ui.input.mode, InputMode::DirectShell);
    handle_input(&mut app, key(KeyCode::Esc));
    assert_eq!(app.ui.input.mode, InputMode::Chat);
}

/// Backspace on an empty command exits the mode.
#[test]
fn test_backspace_empty_exits() {
    let mut app = working_app();
    app.ui.input.mode = InputMode::DirectShell;
    handle_input(&mut app, key(KeyCode::Backspace));
    assert_eq!(app.ui.input.mode, InputMode::Chat);
}

/// Typing appends to the editor and Left moves the cursor without leaving the
/// mode, and Home/End reach the ends of the command.
#[test]
fn test_typing_and_movement() {
    let mut app = working_app();
    app.ui.input.mode = InputMode::DirectShell;
    app.ui.input.editor.set("ab".to_string());
    handle_input(&mut app, key(KeyCode::Char('c')));
    assert_eq!(app.ui.input.editor.value(), "abc");
    handle_input(&mut app, key(KeyCode::Left));
    assert_eq!(app.ui.input.editor.cursor(), 2);
    assert_eq!(app.ui.input.mode, InputMode::DirectShell);
    handle_input(&mut app, key(KeyCode::Home));
    assert_eq!(app.ui.input.editor.cursor(), 0);
    handle_input(&mut app, key(KeyCode::End));
    assert_eq!(app.ui.input.editor.cursor(), 3);
}

/// A cursor move off the end clears the ghost, so a later Tab does not insert
/// the stale suffix at the wrong position.
#[test]
fn test_left_clears_ghost() {
    let mut app = shell_app_with_ghost("cargo t", "est");
    handle_input(&mut app, key(KeyCode::Left));
    assert!(
        app.ui.input.ghost.is_none(),
        "moving the cursor off the end clears the ghost"
    );
    handle_input(&mut app, key(KeyCode::Tab));
    assert_eq!(
        app.ui.input.editor.value(),
        "cargo t",
        "Tab after Left is a no-op, the suffix is not inserted mid-text"
    );
}
