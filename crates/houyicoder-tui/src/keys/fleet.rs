//! Footer-pill fleet selection keys: Shift+Up/Down move the highlighted
//! row. The dispatcher takes disjoint field refs rather than &mut App so it
//! does not add a God-struct broad-access point. Plain arrows fall through
//! to input cursor-edit.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::{App, ViewportMode};

/// The selected child's session id, if a fleet row is highlighted. Resolves
/// Enter-on-fleet: the pill selection takes priority over the cursor's
/// Subagent line when both could drill in.
pub(super) fn selected_fleet_sid(app: &App) -> Option<String> {
    app.fleet_selected
        .and_then(|i| app.fleet.get(i).map(|e| e.agent_id.clone()))
}

/// Shift+Up/Down move the fleet selection. Returns true when the key was
/// consumed so handle_working returns early.
pub(super) fn fleet_shift_selected(
    viewport: ViewportMode,
    fleet_len: usize,
    selected: &mut Option<usize>,
    k: KeyEvent,
) -> bool {
    if viewport != ViewportMode::Working
        || fleet_len == 0
        || !k.modifiers.contains(KeyModifiers::SHIFT)
    {
        return false;
    }
    let delta = match k.code {
        KeyCode::Up => -1,
        KeyCode::Down => 1,
        _ => return false,
    };
    *selected = Some(next_selection(*selected, fleet_len, delta));
    true
}

/// Pure clamp of the fleet selection by a signed delta. A None selection
/// snaps to 0 on the first move; movement clamps at the bounds (no wrap).
fn next_selection(cur: Option<usize>, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    let base = cur.unwrap_or(0) as i32;
    (base + delta).clamp(0, (len - 1) as i32) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// next_selection snaps None to 0 and clamps at the bounds without
    /// wrapping, including the empty-fleet guard.
    #[test]
    fn test_selection_clamps_at_bounds() {
        assert_eq!(next_selection(None, 3, 1), 1);
        assert_eq!(next_selection(Some(2), 3, 1), 2);
        assert_eq!(next_selection(Some(0), 3, -1), 0);
        assert_eq!(next_selection(None, 3, -1), 0);
        assert_eq!(next_selection(None, 0, 1), 0);
    }
}
