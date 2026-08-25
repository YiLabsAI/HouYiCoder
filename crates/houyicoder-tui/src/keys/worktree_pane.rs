//! /worktrees pane key handlers: cursor, enter, remove, search, Esc.
//! Extracted from keys.rs on the file-size gate.

use crate::state::App;
use crate::state::enums::Pane;
use crossterm::event::{KeyCode, KeyEvent};

/// Handle a key when the /worktrees pane is active. Returns true if the key
/// was consumed (the caller should not fall through to generic input).
pub fn handle(app: &mut App, k: KeyEvent) -> bool {
    match k.code {
        KeyCode::Up => {
            app.move_worktree_cursor(-1);
            true
        }
        KeyCode::Down => {
            app.move_worktree_cursor(1);
            true
        }
        KeyCode::Char('d') if app.input.is_empty() => {
            app.remove_worktree_at_cursor();
            true
        }
        KeyCode::Enter if app.input.is_empty() => {
            app.enter_worktree_at_cursor();
            true
        }
        KeyCode::Esc => {
            if app.worktree_list.searching() {
                app.worktree_list.clear_query();
            } else {
                app.pane = Pane::Transcript;
            }
            true
        }
        KeyCode::Char(c) if app.input.is_empty() && (c.is_ascii_graphic() || c == ' ') => {
            app.worktree_list.query.push(c);
            true
        }
        KeyCode::Backspace if app.input.is_empty() => {
            app.worktree_list.query.pop();
            true
        }
        _ => false,
    }
}
