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
