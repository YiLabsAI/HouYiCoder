//! The fleet domain module: the compositions that cross owners, plus the
//! strip state type in a child module. The session's fleet capability holds
//! the child snapshots, the Ui strip state holds the selection and the
//! per-frame drawing facts, and each operation spanning both is one App
//! method so the read of the entries and the write of the strip state cannot
//! drift apart at separate call sites.

pub(crate) mod strip_state;

use std::time::Instant;

use crate::state::App;

impl App {
    /// Step the strip selection by a signed delta against the live entry
    /// count (clamped, no wrap). The /agents pane arrows route here while the
    /// pane renders live children.
    pub(crate) fn move_fleet_selection(&mut self, delta: i32) {
        let len = self.sessions.current.capabilities.fleet.entries.len();
        self.ui.fleet.move_selection(delta, len);
    }

    /// Drop completed children whose grace window elapsed, keeping the child
    /// the user is drilled into (the teammate view names it), then pull the
    /// selection back inside the surviving entries. Returns true when at
    /// least one entry dropped so the caller marks the view dirty. The clamp
    /// runs whether or not anything dropped.
    pub(crate) fn drop_completed_fleet(&mut self) -> bool {
        let retain_viewed = self.ui.teammate_view.as_ref().map(|v| v.child_sid.as_str());
        let fleet = &mut self.sessions.current.capabilities.fleet;
        let dropped = fleet.drop_completed(retain_viewed);
        let len = fleet.entries.len();
        self.ui.fleet.clamp_selected(len);
        dropped
    }

    /// Tick the strip's elapsed-counter pacing: at most one repaint per
    /// second while any child runs, and a reset when none do so the next
    /// spawn ticks immediately. Returns true when the caller should redraw.
    pub(crate) fn tick_fleet_elapsed(&mut self, now: Instant) -> bool {
        let any_running = self.sessions.current.capabilities.fleet.any_running();
        self.ui.fleet.tick_elapsed(now, any_running)
    }
}
