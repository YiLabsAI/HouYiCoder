//! Skill picker modal keys. When the picker is open, Up/Down navigate
//! the filtered list, Enter inserts @skill:name into the input box
//! (the user then presses Enter again to submit, allowing args to be
//! appended), Esc closes without inserting. Chars typed after @ fall
//! through to the input box as a filter query — the picker stays open
//! and the list narrows. A colon signals a namespace prefix
//! (@skill:name) and closes the picker for free-form input. Tab and
//! other navigation keys close the picker and fall through.

use crossterm::event::{KeyCode, KeyEvent};

use crate::state::App;
use crate::view::skill_picker::filtered_skills;

/// Handle a key while the skill picker is open. Returns true when the
/// key was consumed by the picker; false when the key should fall
/// through to the input box (filter chars, Backspace, Tab, colon).
/// The picker stays open on false except for Tab and colon which
/// close it before falling through.
pub(super) fn handle(app: &mut App, k: KeyEvent) -> bool {
    match k.code {
        KeyCode::Up => {
            let cur = app.skill_picker_sel.get();
            app.skill_picker_sel.set(cur.saturating_sub(1));
            true
        }
        KeyCode::Down => {
            let len = filtered_skills(app).len();
            if len > 0 {
                let next = app.skill_picker_sel.get() + 1;
                app.skill_picker_sel.set(next.min(len.saturating_sub(1)));
            }
            true
        }
        KeyCode::Enter => {
            let ordered = filtered_skills(app);
            if let Some(entry) = ordered.get(app.skill_picker_sel.get()) {
                let text = format!("@skill:{}", entry.name);
                app.skill_picker_open = false;
                app.input.set(text);
            } else {
                app.skill_picker_open = false;
            }
            true
        }
        KeyCode::Esc => {
            if app.input.value() == "@" {
                app.input.clear();
            }
            app.skill_picker_open = false;
            true
        }
        KeyCode::Backspace => {
            if app.input.value() == "@" {
                app.input.clear();
                app.skill_picker_open = false;
                true
            } else {
                false
            }
        }
        // A colon signals a namespace prefix (@skill:name, @file:path, etc).
        // Close the picker so the rest of the input is treated as
        // free-form, not a filter query. Skill names are ^[a-z0-9-]+$
        // (no colons), so a colon is an unambiguous namespace signal.
        KeyCode::Char(':') => {
            app.skill_picker_open = false;
            false
        }
        // Tab closes the picker and falls through to cycle_pane, matching
        // the pre-picker behavior where Tab always cycles panes.
        KeyCode::Tab => {
            app.skill_picker_open = false;
            false
        }
        _ => false,
    }
}
