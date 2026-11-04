//! Key routing while the composer is in DirectShell mode.
//!
//! A shell command is edited in the same box as a chat draft but answers a
//! different set of keys: Enter runs it instead of sending a message, Tab
//! accepts the ghost suffix instead of cycling a pane, and an empty Backspace
//! leaves the mode. Keeping the two routers apart is what lets each one state
//! its own rules without the other's conditions in the way.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::Instant;

use crate::input::InputMode;
use crate::state::App;

/// Handle a keystroke while the composer is in DirectShell mode. Tab and Right
/// accept the ghost suffix without running; Enter runs the typed command (the
/// ghost is never swallowed into it); Esc drops the ghost first and exits the
/// mode on a second press; Backspace on an empty draft exits; typing refreshes
/// the ghost.
pub(super) fn handle_direct_shell(app: &mut App, k: KeyEvent) {
    let now = Instant::now();
    match k.code {
        KeyCode::Tab | KeyCode::Right if app.ui.input.ghost.is_some() => {
            app.ui.input.accept_ghost(now);
        }
        KeyCode::Enter => {
            // A refused submission leaves the composer as it is: the command
            // stays in the box in shell mode, so the user can retry it once
            // the work blocking it finishes.
            if let Some(command) = app.ui.input.shell_command()
                && app.submit_direct_shell(command.clone())
            {
                app.ui.input.record_shell_command(&command);
                app.ui.input.editor = Default::default();
                app.ui.input.mode = InputMode::Chat;
                app.ui.input.ghost = None;
            }
        }
        KeyCode::Esc => {
            if app.ui.input.ghost.is_some() {
                app.ui.input.clear_ghost();
            } else {
                app.ui.input.exit_shell_mode();
            }
        }
        KeyCode::Backspace => {
            if app.ui.input.editor.is_empty() {
                app.ui.input.exit_shell_mode();
            } else {
                app.ui.input.editor.pop();
                app.ui.input.refresh_ghost(now);
            }
        }
        // The same rule as the chat router, stated once per router: only an
        // unmodified character is command text.
        KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.ui.input.editor.push(c);
            app.ui.input.refresh_ghost(now);
        }
        KeyCode::Left => {
            app.ui.input.editor.move_left();
            app.ui.input.refresh_ghost(now);
        }
        KeyCode::Home => {
            app.ui.input.editor.move_home();
            app.ui.input.refresh_ghost(now);
        }
        KeyCode::End => {
            app.ui.input.editor.move_end();
            app.ui.input.refresh_ghost(now);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
