//! Input-editor key behavior on the working screen: cursor movement across
//! graphemes and wrapped lines, whole-text jumps, and line-kill. The wrapped
//! movement reads the column count the draw pass feeds back, so the cursor
//! walks the same wrap the render shows.

use super::handle_input;
use crate::composition;
use crate::state::{App, Screen};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
