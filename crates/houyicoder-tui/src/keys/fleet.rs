//! Footer-strip fleet selection keys: Shift+Up/Down move the highlighted
//! row. The dispatcher takes the strip state and the live entry count (not
//! &mut App) so it does not add a God-struct broad-access point. Plain
//! arrows fall through to input cursor-edit.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::fleet::strip_state::FleetStripState;
use crate::state::{App, ViewportMode};

/// The selected child's session id, if a fleet row is highlighted. Resolves
/// Enter-on-fleet: the strip selection takes priority over the cursor's
/// Subagent line when both could drill in.
pub(super) fn selected_fleet_sid(app: &App) -> Option<String> {
    app.ui.fleet.selected.and_then(|i| {
        app.sessions
            .current
            .capabilities
            .fleet
            .entries
            .get(i)
            .map(|e| e.agent_id.clone())
    })
}

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
    strip.selected = Some(next_selection(strip.selected, len, delta));
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
