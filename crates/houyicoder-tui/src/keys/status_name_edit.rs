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
    let Some(field) = app.ui.navigation.status_pane.name_edit.as_mut() else {
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
mod tests;
