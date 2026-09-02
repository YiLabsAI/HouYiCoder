//! Skill picker modal keys. When the picker is open it swallows all keys:
//! Up/Down navigate, Enter inserts @skill:name into the input box (the user
//! then presses Enter again to submit, allowing args to be appended), Esc
//! closes without inserting. Any unrecognized key closes the picker and
//! falls through so no char is silently lost.

use crossterm::event::{KeyCode, KeyEvent};

use crate::state::App;
use crate::view::skills_pane::display_order;

/// Handle a key while the skill picker is open. Returns true when the key
/// was consumed; false when the key should fall through after closing the
/// picker (a typed char the user will want in the input box).
pub(super) fn handle(app: &mut App, k: KeyEvent) -> bool {
    match k.code {
        KeyCode::Up => {
            let cur = app.skill_picker_sel.get();
            app.skill_picker_sel.set(cur.saturating_sub(1));
            true
        }
        KeyCode::Down => {
            let len = display_order(&app.skill_entries).len();
            if len > 0 {
                let next = app.skill_picker_sel.get() + 1;
                app.skill_picker_sel.set(next.min(len.saturating_sub(1)));
            }
            true
        }
        KeyCode::Enter => {
            let ordered = display_order(&app.skill_entries);
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
            // If the input holds only the lone @ from the trigger, clear it
            // so Esc restores the pre-picker empty state.
            if app.input.value() == "@" {
                app.input.clear();
            }
            app.skill_picker_open = false;
            true
        }
        _ => false,
    }
}
