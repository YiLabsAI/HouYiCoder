//! /worktrees pane key handlers: cursor, detail, enter, search, Esc.
//! Extracted from keys.rs on the file-size gate. The pane is display-only
//! per the U5b design (list + Level 1 detail, no remove); 'e' in the
//! detail view opens the worktree (the enter ability, kept beyond the
//! design's display-only round).

use crate::state::App;
use crate::state::enums::Pane;
use crossterm::event::{KeyCode, KeyEvent};

/// Handle a key when the /worktrees pane is active. Returns true if the key
/// was consumed (the caller should not fall through to generic input).
pub fn handle(app: &mut App, k: KeyEvent) -> bool {
    let level = app.worktree_level.get();
    match k.code {
        KeyCode::Up if level == 0 => {
            app.move_worktree_cursor(-1);
            true
        }
        KeyCode::Down if level == 0 => {
            app.move_worktree_cursor(1);
            true
        }
        // Enter on the list opens the Level 1 detail.
        KeyCode::Enter if level == 0 && app.input.is_empty() => {
            app.worktree_level.set(1);
            true
        }
        // 'e' in the detail view opens the worktree (the enter ability).
        // Only at Level 1 so it never conflicts with search typing on the
        // list (where 'e' is a normal query character).
        KeyCode::Char('e') if level == 1 && app.input.is_empty() => {
            app.worktree_level.set(0);
            app.enter_worktree_at_cursor();
            true
        }
        KeyCode::Esc => {
            if level > 0 {
                app.worktree_level.set(0);
            } else if app.worktree_list.searching() {
                app.worktree_list.clear_query();
            } else {
                app.pane = Pane::Transcript;
            }
            true
        }
        KeyCode::Char(c)
            if level == 0 && app.input.is_empty() && (c.is_ascii_graphic() || c == ' ') =>
        {
            app.worktree_list.query.push(c);
            true
        }
        KeyCode::Backspace if level == 0 && app.input.is_empty() => {
            app.worktree_list.query.pop();
            true
        }
        _ => false,
    }
}
