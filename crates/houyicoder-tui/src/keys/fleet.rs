//! Footer-strip fleet selection keys: Shift+Up/Down move the highlighted
//! row. The dispatcher takes the strip state and the live entry count (not
//! &mut App) so it does not add a God-struct broad-access point. Plain
//! arrows fall through to input cursor-edit.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::agent_message::FleetEntry;
use crate::fleet::strip_state::FleetStripState;
use crate::state::{App, ViewportMode};

/// Shift+Up/Down move the fleet selection. Returns true when the key was
/// consumed so handle_working returns early.
pub(super) fn fleet_shift_selected(
    strip: &mut FleetStripState,
    len: usize,
    viewport: ViewportMode,
    k: KeyEvent,
) -> bool {
    if viewport != ViewportMode::Working || len == 0 || !k.modifiers.contains(KeyModifiers::SHIFT) {
        return false;
    }
    let delta = match k.code {
        KeyCode::Up => -1,
        KeyCode::Down => 1,
        _ => return false,
    };
    strip.focus(next_selection(strip.selected(), len, delta));
    true
}

/// Pure clamp of the fleet selection by a signed delta. A None selection
/// snaps to 0 on the first move; movement clamps at the bounds (no wrap).
fn next_selection(cur: Option<usize>, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    let Some(base) = cur else {
        return 0;
    };
    (base as i32 + delta).clamp(0, (len - 1) as i32) as usize
}

#[cfg(test)]
mod tests;

/// What the strip decided to do with a key while it holds the keyboard.
///
/// The decision is a value rather than an effect so the strip's key handling
/// stays a pure function of the strip state and the child list: the caller
/// that owns the App performs the one action that needs it.
pub(super) enum FleetKey {
    /// The key belongs to the draft: a printable character is the user
    /// changing their mind about where they are.
    PassThrough,
    /// The strip took the key and keeps the keyboard. Arrow navigation stays
    /// here, so a second press moves again instead of falling back into the
    /// draft's history recall.
    Stay,
    /// The strip took the key and gives the keyboard back: Esc, or Up on the
    /// first row.
    Leave,
    /// Open this child.
    Open(String),
}

/// Handle a key while the strip holds the keyboard.
///
/// The strip is a list of children, so it follows the list grammar every other
/// pane uses: Up/Down move, Enter opens, Esc returns one level. Returning to
/// the input box keeps the highlighted child, so the user can come back.
/// A printable character leaves the strip and lands in the draft, because
/// typing is the user changing their mind about where they are.
pub(super) fn handle_focused(
    strip: &mut FleetStripState,
    entries: &[FleetEntry],
    k: KeyEvent,
) -> FleetKey {
    let len = entries.len();
    if len == 0 {
        return FleetKey::PassThrough;
    }
    let current = strip.selected().unwrap_or(0).min(len - 1);
    match k.code {
        // At the top there is nothing above to select, and the input box is
        // the surface above: Up is the way back, symmetric with Down being
        // the way in.
        KeyCode::Up if current == 0 => FleetKey::Leave,
        KeyCode::Up => {
            strip.set_selected(Some(current - 1));
            FleetKey::Stay
        }
        KeyCode::Down => {
            strip.set_selected(Some((current + 1).min(len - 1)));
            FleetKey::Stay
        }
        KeyCode::Enter => FleetKey::Open(entries[current].agent_id.clone()),
        KeyCode::Esc => FleetKey::Leave,
        // A printable character or an edit/cursor key belongs to the draft.
        // Leaving the strip here is what makes typing or editing a draft feel
        // natural rather than fighting a widget that swallowed the keystroke.
        KeyCode::Char(_)
        | KeyCode::Backspace
        | KeyCode::Delete
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Home
        | KeyCode::End => FleetKey::PassThrough,
        _ => FleetKey::Stay,
    }
}

/// Move the keyboard into the child strip, if it has a row to land on. The
/// selection snaps to the first running child so the user lands on something
/// rather than on a stale index.
pub(super) fn focus_strip(strip: &mut FleetStripState, len: usize) -> bool {
    if len == 0 {
        return false;
    }
    strip.focus(strip.selected().unwrap_or(0).min(len - 1));
    true
}

impl App {
    /// Run the strip's key decision and apply the one action that needs the
    /// whole shell. The decision itself stays a pure function of the strip
    /// state and the child list; only opening a child reaches across owners,
    /// which is why that half is a method on the owner rather than a free
    /// function taking the shell.
    pub(crate) fn dispatch_fleet_key(&mut self, k: KeyEvent) -> bool {
        let decision = handle_focused(
            &mut self.ui.fleet,
            &self.sessions.current.capabilities.fleet.entries,
            k,
        );
        match decision {
            FleetKey::Stay => true,
            FleetKey::PassThrough | FleetKey::Leave => {
                self.ui.fleet.clear_focus();
                !matches!(decision, FleetKey::PassThrough)
            }
            FleetKey::Open(sid) => {
                self.ui.fleet.clear_focus();
                self.enter_teammate_view_for_sid(&sid, true);
                true
            }
        }
    }
}
