//! The idle wake pacing for pending completion notifications.
//!
//! An idle session that the server says holds endings the parent model has
//! not read wakes itself on a debounce so near-simultaneous children batch
//! into one turn, and only a bounded number of turns in a row, so the wake
//! loop cannot outrun the user's own next turn.

use std::time::{Duration, Instant};

/// How long the pending count must stay stable before the idle session
/// wakes itself: nearly-simultaneous background endings batch into one turn
/// instead of waking per child.
pub(crate) const WAKE_DEBOUNCE: Duration = Duration::from_millis(500);

/// How many consecutive notification turns the session may start on its own.
/// Bounds the wake loop a child that keeps finishing (or a server that keeps
/// re-reporting) could otherwise drive; any user input resets the budget.
pub(crate) const MAX_AUTO_WAKE_TURNS: u32 = 5;

/// The pacing state for the idle wake.
#[derive(Default)]
pub(crate) struct NotificationWake {
    /// The count the current debounce window opened on. None while no window
    /// runs. A count that changes mid-window restarts the window, so the wake
    /// always consumes the settled figure rather than a snapshot the next
    /// event invalidates.
    pub(crate) pending_at_window: Option<u32>,
    /// When the current debounce window started.
    pub(crate) window_started: Option<Instant>,
    /// Consecutive notification turns this session started on its own.
    pub(crate) auto_wake_turns: u32,
}

impl NotificationWake {
    /// Record that the user produced input: the user's own turn always goes
    /// first, so the consecutive budget restarts and any open window closes.
    pub(crate) fn user_interrupted(&mut self) {
        self.auto_wake_turns = 0;
        self.pending_at_window = None;
        self.window_started = None;
    }
}
