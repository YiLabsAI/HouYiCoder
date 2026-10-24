//! Interaction and per-frame drawing facts for the footer fleet strip.
//!
//! The child snapshots the selection indexes are the session's fleet
//! capability; this state owns what the strip does with them on screen: the
//! highlighted row and the repaint pacing of the live elapsed counters. The
//! geometry a draw produced (where the strip landed, how many rows the footer
//! budget granted it) is that frame's, so it lives in the published frame's
//! layout snapshot. The compositions across the two owners are App methods.

use std::time::{Duration, Instant};

/// Where the strip's cursor is, and therefore who owns the keyboard.
///
/// The highlight and the keyboard are one fact, not two: a row can be
/// highlighted while the input box still has the keys (the strip is showing
/// which child the /agents pane points at), and a focused strip always has a
/// row. Two separate fields would let those disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StripCursor {
    /// The input box owns the keyboard; this child, if any, stays highlighted.
    Idle(Option<usize>),
    /// The strip owns the keyboard, on this row.
    Focused(usize),
}

impl Default for StripCursor {
    fn default() -> Self {
        Self::Idle(None)
    }
}

impl StripCursor {
    /// The highlighted child, whether or not the strip owns the keyboard.
    pub(crate) fn selected(self) -> Option<usize> {
        match self {
            Self::Idle(row) => row,
            Self::Focused(row) => Some(row),
        }
    }

    /// Whether the strip owns the keyboard.
    pub(crate) fn is_focused(self) -> bool {
        matches!(self, Self::Focused(_))
    }

    /// Move the strip's cursor to this row, keeping who owns the keyboard.
    pub(crate) fn with_row(self, row: Option<usize>) -> Self {
        match (self, row) {
            (_, None) => Self::Idle(None),
            (Self::Focused(_), Some(row)) => Self::Focused(row),
            (Self::Idle(_), Some(row)) => Self::Idle(Some(row)),
        }
    }
}

#[derive(Default)]
pub(crate) struct FleetStripState {
    /// The highlight and who owns the keyboard, as one value. The /agents pane
    /// keeps its own cursor and writes this when that cursor lands on a
    /// running row, so the two highlights name the same child.
    pub(crate) cursor: StripCursor,
    /// When the strip last re-rendered for the elapsed counters. The poll
    /// loop ticks this at most once per second when running children exist.
    last_elapsed_tick: Option<Instant>,
}

impl FleetStripState {
    /// Pull a selection that points past the surviving entries back into
    /// bounds: a dropped selected row clears the selection when nothing
    /// survives, else lands on the last row.
    pub(crate) fn clamp_selected(&mut self, len: usize) {
        let row = self.cursor.selected().map(|s| s.min(len.saturating_sub(1)));
        self.cursor = self.cursor.with_row(row.filter(|_| len > 0));
    }

    /// The highlighted child.
    pub(crate) fn selected(&self) -> Option<usize> {
        self.cursor.selected()
    }

    /// Whether the strip owns the keyboard.
    pub(crate) fn is_focused(&self) -> bool {
        self.cursor.is_focused()
    }

    /// Move the highlight without taking the keyboard.
    pub(crate) fn set_selected(&mut self, row: Option<usize>) {
        self.cursor = self.cursor.with_row(row);
    }

    /// Take the keyboard on this row.
    pub(crate) fn focus(&mut self, row: usize) {
        self.cursor = StripCursor::Focused(row);
    }

    /// Give the keyboard back to the input box, keeping the highlight.
    pub(crate) fn clear_focus(&mut self) {
        self.cursor = StripCursor::Idle(self.cursor.selected());
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
