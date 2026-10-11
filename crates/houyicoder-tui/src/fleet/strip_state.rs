//! Interaction and per-frame drawing facts for the footer fleet strip.
//!
//! The child snapshots the selection indexes are the session's fleet
//! capability; this state owns what the strip does with them on screen: the
//! highlighted row, what the last draw granted and where it landed, and the
//! repaint pacing of the live elapsed counters. The compositions across the
//! two owners are App methods.

use std::cell::Cell;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;

#[derive(Default)]
pub(crate) struct FleetStripState {
    /// The Shift-arrow selection: which child row is highlighted, when the
    /// user has moved one. Doubles as the /agents pane cursor while live
    /// children exist, since the pane renders the same rows.
    pub(crate) selected: Option<usize>,
    /// Rows the footer budget granted this frame, published by the draw pass.
    /// The status-bar hint reads it to advertise the affordance the strip can
    /// actually honor: per-row selection while rows are visible, the pane
    /// when the strip is a summary or gone.
    pub(crate) granted: Cell<u16>,
    /// Where the strip was drawn this frame, for the mouse router. Zero when
    /// not drawn, so a stale rect cannot swallow clicks.
    pub(crate) rect: Cell<Rect>,
    /// When the strip last re-rendered for the elapsed counters. The poll
    /// loop ticks this at most once per second when running children exist.
    last_elapsed_tick: Option<Instant>,
}

impl FleetStripState {
    /// Move the selection by a signed delta (clamped, no wrap) against the
    /// entry count the caller read from the session's fleet capability. Used
    /// by the /agents pane Up/Down navigation — the working-screen footer
    /// uses Shift+arrows via fleet_shift_selected instead.
    pub(crate) fn move_selection(&mut self, delta: i32, len: usize) {
        if len == 0 {
            return;
        }
        let base = self.selected.unwrap_or(0) as i32;
        self.selected = Some((base + delta).clamp(0, (len - 1) as i32) as usize);
    }

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
