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
            let cur = app.ui.navigation.skill_picker.sel.get();
            app.ui
                .navigation
                .skill_picker
                .sel
                .set(cur.saturating_sub(1));
            true
        }
        KeyCode::Down => {
            let len = filtered_skills(app).len();
            if len > 0 {
                let next = app.ui.navigation.skill_picker.sel.get() + 1;
                app.ui
                    .navigation
                    .skill_picker
                    .sel
                    .set(next.min(len.saturating_sub(1)));
            }
            true
        }
        KeyCode::Enter => {
            let ordered = filtered_skills(app);
            if let Some(entry) = ordered.get(app.ui.navigation.skill_picker.sel.get()) {
                let text = format!("@skill:{}", entry.name);
                app.ui.navigation.skill_picker.open = false;
                app.ui.input.editor.set(text);
            } else {
                app.ui.navigation.skill_picker.open = false;
            }
            true
        }
        KeyCode::Esc => {
            if app.ui.input.editor.value() == "@" {
                app.ui.input.editor.clear();
            }
            app.ui.navigation.skill_picker.open = false;
            true
        }
        KeyCode::Backspace => {
            if app.ui.input.editor.value() == "@" {
                app.ui.input.editor.clear();
                app.ui.navigation.skill_picker.open = false;
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
            app.ui.navigation.skill_picker.open = false;
            false
        }
        // Tab closes the picker and falls through to cycle_pane, matching
        // the pre-picker behavior where Tab always cycles panes.
        KeyCode::Tab => {
            app.ui.navigation.skill_picker.open = false;
            false
        }
        _ => false,
    }
}
