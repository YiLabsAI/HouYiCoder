//! Input-editor key behavior on the working screen: cursor movement across
//! graphemes and wrapped lines, whole-text jumps, and line-kill. The wrapped
//! movement reads the column count the draw pass feeds back, so the cursor
//! walks the same wrap the render shows.

use super::handle_input;
use crate::composition;
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

/// Left and Right move the cursor one grapheme at a time within the text,
/// rather than jumping to a line or pane boundary.
#[test]
fn test_left_right_move_cursor() {
    let mut app = working_app();
    app.ui.input.editor.set("ab".to_string());
    handle_input(&mut app, key(KeyCode::Left));
    assert_eq!(
        app.ui.input.editor.cursor(),
        1,
        "Left moves one grapheme back"
    );
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(
        app.ui.input.editor.cursor(),
        2,
        "Right moves one grapheme forward"
    );
}

/// Ctrl+A and Ctrl+E park at the whole text's start and end readline-style,
/// ignoring the wrapped-line boundaries Home and End respect.
#[test]
fn test_ctrl_jump_whole_text() {
    let mut app = working_app();
    app.ui.input.editor.set("hello".to_string());
    handle_input(
        &mut app,
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
    );
    assert_eq!(app.ui.input.editor.cursor(), 0, "Ctrl+A parks at the start");
    handle_input(
        &mut app,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
    );
    assert_eq!(app.ui.input.editor.cursor(), 5, "Ctrl+E parks at the end");
}

/// Up/Down move the cursor across wrapped input lines using the column count
/// the draw pass fed back, so the cursor walks the same wrap the render shows
/// rather than jumping to the text start.
#[test]
fn test_up_down_walk_wrap() {
    let mut app = working_app();
    app.ui.input.editor.set("word ".repeat(40));
    let bottom = app.ui.input.editor.cursor();
    handle_input(&mut app, key(KeyCode::Up));
    let up = app.ui.input.editor.cursor();
    handle_input(&mut app, key(KeyCode::Down));
    let down = app.ui.input.editor.cursor();
    assert!(up < bottom, "Up walks toward the top of the wrap");
    assert!(down > up, "Down walks back toward the bottom");
}

/// Home/End land on the current wrapped line's start and end when the text
/// wraps, instead of the whole text's boundaries, so the cursor follows the
/// same wrap the render draws.
#[test]
fn test_home_end_wrap_line() {
    let mut app = working_app();
    app.ui.input.editor.set("word ".repeat(40));
    let end = app.ui.input.editor.cursor();
    handle_input(&mut app, key(KeyCode::Home));
    let home = app.ui.input.editor.cursor();
    assert!(
        home > 0,
        "Home lands on the wrapped line start, not the text start"
    );
    assert!(home < end, "Home leaves the text end");
    handle_input(&mut app, key(KeyCode::End));
    assert_eq!(
        app.ui.input.editor.cursor(),
        end,
        "End returns to the wrapped line end"
    );
}

/// Ctrl+U deletes backward to the wrapped line's start, leaving the earlier
/// wrapped lines in the box rather than clearing the whole buffer.
#[test]
fn test_ctrl_u_kills_line() {
    let mut app = working_app();
    app.ui.input.editor.set("word ".repeat(40));
    let before = app.ui.input.editor.value().len();
    handle_input(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    );
    assert!(
        app.ui.input.editor.value().len() < before,
        "Ctrl+U deletes to the wrapped line start"
    );
}

/// Ctrl+U at the start of a wrapped line has nothing behind it to delete, so
/// the readline meaning alone leaves the key dead at exactly the moment the
/// draft should go. It clears the field instead, which is the whole-buffer
/// reset the multi-line prompt needed.
#[test]
fn test_ctrl_u_clears_whole() {
    let mut app = working_app();
    app.ui
        .input
        .editor
        .set("first line\nsecond line".to_string());
    app.ui.input.editor.move_to(0);
    handle_input(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    );
    assert!(
        app.ui.input.editor.is_empty(),
        "a dead backward kill clears the field: {:?}",
        app.ui.input.editor.value()
    );
}

/// The readline meaning still wins where it applies: with text behind the
/// cursor the key deletes back to the line start and leaves the earlier lines
/// alone. Without this the escalation above would be a whole-buffer clear.
#[test]
fn test_ctrl_u_readline_offset() {
    let mut app = working_app();
    app.ui.input.editor.set("keep me\ndrop this".to_string());
    app.ui
        .input
        .editor
        .move_to("keep me\n".len() + "drop ".len());
    handle_input(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    );
    assert_eq!(
        app.ui.input.editor.value(),
        "keep me\nthis",
        "only the text between the line start and the cursor goes"
    );
}

/// Ctrl+K deletes forward to the end of the wrapped line and keeps the
/// newline, so the line shortens rather than the following line moving up
/// into it.
#[test]
fn test_ctrl_k_kills_forward() {
    let mut app = working_app();
    app.ui.input.editor.set("keep this\ndrop that".to_string());
    app.ui
        .input
        .editor
        .move_to("keep this\n".len() + "drop".len());
    handle_input(
        &mut app,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
    );
    assert_eq!(
        app.ui.input.editor.value(),
        "keep this\ndrop",
        "the rest of the line goes and the next line stays put"
    );
}

/// The two kills are complements: Ctrl+U takes what is behind the cursor on
/// the line and Ctrl+K takes what is ahead of it, both leaving the other
/// lines untouched.
#[test]
fn test_kill_pair_partitions_line() {
    let mut app = working_app();
    app.ui.input.editor.set("before\nafter".to_string());
    app.ui.input.editor.move_to(0);
    handle_input(
        &mut app,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
    );
    assert_eq!(
        app.ui.input.editor.value(),
        "\nafter",
        "Ctrl+K emptied the first line and kept the second"
    );
}

/// ATTACK: the caret at the very first column of a multi-line draft. Readline
/// Ctrl+U has nothing behind it there, and the escalation must clear the whole
/// buffer rather than leaving the draft untouched.
#[test]
fn test_ctrl_u_clears_buffer() {
    let mut app = working_app();
    app.ui
        .input
        .editor
        .set("first line\nsecond line".to_string());
    app.ui.input.editor.move_to(0);
    handle_input(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    );
    assert!(
        app.ui.input.editor.is_empty(),
        "Ctrl+U at column zero clears the buffer: {:?}",
        app.ui.input.editor.value()
    );
}

/// The caret at the start of a middle line deletes the newline behind it and
/// nothing else. The line above holds text the user did not point at, so a
/// whole-buffer clear here would throw that work away; the escalation to a
/// full clear belongs to offset zero, where nothing is behind the cursor.
#[test]
fn test_ctrl_u_keeps_above() {
    let mut app = working_app();
    app.ui.input.editor.set("keep me\ndrop this".to_string());
    app.ui.input.editor.move_to("keep me\n".len());
    handle_input(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    );
    assert_eq!(
        app.ui.input.editor.value(),
        "keep medrop this",
        "the text above survives; only the newline behind the cursor goes"
    );
}

use crate::input::InputMode;

/// A working app with the composer in DirectShell mode holding a ghost suffix.
fn shell_app_with_ghost(draft: &str, suffix: &str) -> App {
    let mut app = working_app();
    app.ui.input.mode = InputMode::DirectShell;
    app.ui.input.editor.set(draft.to_string());
    app.ui.input.ghost = Some(suffix.to_string());
    app
}

/// Tab in DirectShell mode accepts the ghost suffix into the editor without
/// running the command.
#[test]
fn test_shell_tab_accepts_ghost() {
    let mut app = shell_app_with_ghost("cargo t", "est");
    handle_input(&mut app, key(KeyCode::Tab));
    assert_eq!(app.ui.input.editor.value(), "cargo test");
    assert!(app.ui.input.ghost.is_none());
    assert_eq!(app.ui.input.mode, InputMode::DirectShell);
}

/// Enter in DirectShell mode sends the typed command (never the ghost), clears
/// the editor, and returns the composer to Chat once the host took it.
#[test]
fn test_shell_enter_runs_typed() {
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
fn test_shell_enter_keeps_refused() {
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

/// Esc drops the ghost first and stays in DirectShell; a second Esc exits.
#[test]
fn test_shell_esc_clears_exits() {
    let mut app = shell_app_with_ghost("cargo t", "est");
    handle_input(&mut app, key(KeyCode::Esc));
    assert!(app.ui.input.ghost.is_none());
    assert_eq!(app.ui.input.mode, InputMode::DirectShell);
    handle_input(&mut app, key(KeyCode::Esc));
    assert_eq!(app.ui.input.mode, InputMode::Chat);
}

/// Backspace on an empty DirectShell draft exits the mode.
#[test]
fn test_shell_backspace_empty_exits() {
    let mut app = working_app();
    app.ui.input.mode = InputMode::DirectShell;
    handle_input(&mut app, key(KeyCode::Backspace));
    assert_eq!(app.ui.input.mode, InputMode::Chat);
}

/// A leading dollar plus a space in Chat mode enters DirectShell and consumes
/// the prefix; a dollar followed by a digit stays Chat.
#[test]
fn test_dollar_space_enters_shell() {
    let mut app = working_app();
    app.ui.input.editor.set("$".to_string());
    handle_input(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(app.ui.input.mode, InputMode::DirectShell);
    let mut app = working_app();
    app.ui.input.editor.set("$".to_string());
    handle_input(&mut app, key(KeyCode::Char('1')));
    assert_eq!(app.ui.input.mode, InputMode::Chat);
}

/// Typing in DirectShell mode appends to the editor and refreshes the ghost; Left
/// moves the cursor without leaving the mode.
#[test]
fn test_shell_typing_and_left() {
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

/// A pasted dollar plus a space enters DirectShell mode the same way a typed one
/// does, so paste does not bypass the mode switch.
#[test]
fn test_paste_enters_shell_mode() {
    let mut app = working_app();
    app.apply_paste_token("$ cargo test");
    assert_eq!(app.ui.input.mode, InputMode::DirectShell);
    assert_eq!(app.ui.input.editor.value(), "cargo test");
}

/// A cursor move off the end clears the ghost, so a later Tab does not insert the
/// stale suffix at the wrong position.
#[test]
fn test_shell_left_clears_ghost() {
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
