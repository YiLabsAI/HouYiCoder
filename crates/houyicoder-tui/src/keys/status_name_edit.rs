//! Status-name edit keys (the e-to-rename flow on the /status Status tab).
//! Extracted from keys.rs to keep that module under the file-size gate. See
//! keys::handle_working for the dispatch routing. Enter commits a
//! RenameSession request; Esc cancels; the rest edit the buffer.

use crossterm::event::{KeyCode, KeyEvent};

use crate::state::App;

/// Handle a key while the /status Status tab name editor is open. Chars
/// edit the buffer, Enter commits (ships the request), Esc cancels, arrows
/// move the caret. Returns nothing; the caller returns after this (the
/// editor is a focused input mode that swallows all keys).
pub(super) fn handle_status_name_edit(app: &mut App, k: KeyEvent) {
    let Some(field) = app.status_name_edit.as_mut() else {
        return;
    };
    match k.code {
        KeyCode::Enter => app.commit_status_name_edit(),
        KeyCode::Esc => app.cancel_status_name_edit(),
        KeyCode::Backspace => field.backspace(),
        KeyCode::Left => field.move_left(),
        KeyCode::Right => field.move_right(),
        KeyCode::Char(c) if !c.is_control() => field.insert_str(&c.to_string()),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::super::handle_working;
    use crate::input::InputField;
    use crate::state::{App, Pane, Screen, ViewportMode};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A working app parked on the /status Status tab with an empty input box,
    /// the state the e key opens the editor from.
    fn status_tab_app() -> App {
        let mut app = crate::test_support::working_app();
        app.screen = Screen::Working;
        app.viewport = ViewportMode::Working;
        app.pane = Pane::Status;
        app.status_tab = crate::state::enums::StatusTab::Status;
        app
    }

    /// The e key on the Status tab opens the inline name editor.
    #[test]
    fn test_e_opens_editor() {
        let mut app = status_tab_app();
        handle_working(&mut app, key(KeyCode::Char('e')));
        assert!(app.status_name_edit.is_some(), "editor opened");
    }

    /// e while editing does NOT open a second editor (the routing block
    /// intercepts it first); it inserts 'e' into the buffer like any other char.
    #[test]
    fn test_e_inserts_while_editing() {
        let mut app = status_tab_app();
        app.status_name_edit = Some(InputField::new());
        handle_working(&mut app, key(KeyCode::Char('e')));
        assert_eq!(
            app.status_name_edit.as_ref().unwrap().value(),
            "e",
            "e while editing types into the buffer, not opens a new editor"
        );
    }

    /// Typing while the editor is open routes chars into the buffer, not the
    /// generic input box or tab cycling.
    #[test]
    fn test_chars_edit_buffer() {
        let mut app = status_tab_app();
        app.status_name_edit = Some(InputField::new());
        handle_working(&mut app, key(KeyCode::Char('h')));
        handle_working(&mut app, key(KeyCode::Char('i')));
        let field = app.status_name_edit.as_ref().expect("editor open");
        assert_eq!(field.value(), "hi", "chars landed in the buffer");
    }

    /// Backspace deletes from the buffer while editing.
    #[test]
    fn test_backspace_edits_buffer() {
        let mut app = status_tab_app();
        let mut f = InputField::new();
        f.insert_str("ab");
        app.status_name_edit = Some(f);
        handle_working(&mut app, key(KeyCode::Backspace));
        assert_eq!(
            app.status_name_edit.as_ref().unwrap().value(),
            "a",
            "backspace trimmed"
        );
    }

    /// Esc cancels the editor: no request shipped, the buffer is dropped.
    #[test]
    fn test_esc_cancels_editor() {
        let mut app = status_tab_app();
        app.status_name_edit = Some(InputField::new());
        handle_working(&mut app, key(KeyCode::Esc));
        assert!(app.status_name_edit.is_none(), "editor closed on Esc");
    }

    /// Enter commits: with no session wired (stub), the commit reports stub
    /// mode + drops the editor (does not panic).
    #[test]
    fn test_enter_commits_no_session() {
        let mut app = status_tab_app();
        app.session = None;
        let mut f = InputField::new();
        f.insert_str("x");
        app.status_name_edit = Some(f);
        handle_working(&mut app, key(KeyCode::Enter));
        assert!(app.status_name_edit.is_none(), "editor closed on commit");
    }
}
