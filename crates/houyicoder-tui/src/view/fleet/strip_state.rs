//! Interaction and per-frame drawing facts for the footer fleet strip.
//!
//! The child snapshots the selection indexes are the session's fleet
//! capability; this state owns what the strip does with them on screen: the
//! highlighted row and the repaint pacing of the live elapsed counters. The
//! geometry a draw produced (where the strip landed, how many rows the footer
//! budget granted it) is that frame's, so it lives in the published frame's
//! layout snapshot. The compositions across the two owners are App methods.

use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct FleetStripState {
    /// The Shift-arrow selection: which child row is highlighted, when the
    /// user has moved one. The /agents pane keeps its own cursor and writes
    /// this when that cursor lands on a running row, so the two highlights
    /// name the same child.
    pub(crate) selected: Option<usize>,
    /// When the strip last re-rendered for the elapsed counters. The poll
    /// loop ticks this at most once per second when running children exist.
    last_elapsed_tick: Option<Instant>,
}

impl FleetStripState {
    /// Pull a selection that points past the surviving entries back into
    /// bounds: a dropped selected row clears the selection when nothing
    /// survives, else lands on the last row.
    pub(crate) fn clamp_selected(&mut self, len: usize) {
        match self.selected {
            Some(s) if s >= len => self.selected = (len > 0).then(|| len - 1),
            _ => {}
        }
    }

    /// Tick the live elapsed counters: return true when the strip should
    /// re-render (at most once per second while running children exist, per
    /// the flag the caller read from the session's fleet capability). Resets
    /// when no children run so the next spawn ticks immediately.
    pub(crate) fn tick_elapsed(&mut self, now: Instant, any_running: bool) -> bool {
        if !any_running {
            self.last_elapsed_tick = None;
            return false;
        }
        match self.last_elapsed_tick {
            Some(last) if now.duration_since(last) < Duration::from_secs(1) => false,
            _ => {
                self.last_elapsed_tick = Some(now);
                true
            }
        }
    }
}
