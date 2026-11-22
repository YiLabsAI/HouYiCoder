//! Pacing for tool progress sent to an ACP client. A tool reports progress on
//! its own cadence, which can be far faster than a client can render, so a
//! call's updates are sent at most once per interval and the call's completion
//! carries the final state.

use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

/// The shortest gap between two progress updates for one call.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

/// Tracks when each call last reported progress, so a burst of snapshots
/// becomes one update per interval.
pub(super) struct ToolProgressThrottle {
    interval: Duration,
    last_sent: HashMap<String, Instant>,
}

impl Default for ToolProgressThrottle {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolProgressThrottle {
    /// Build a throttle with the standard interval between updates.
    pub(super) fn new() -> Self {
        Self {
            interval: PROGRESS_INTERVAL,
            last_sent: HashMap::new(),
        }
    }

    /// Whether a progress update for this call should be sent at this
    /// instant. The first update for a call always passes.
    pub(super) fn allow(&mut self, call_id: &str, now: Instant) -> bool {
        match self.last_sent.get(call_id) {
            Some(last) if now.saturating_duration_since(*last) < self.interval => false,
            _ => {
                self.last_sent.insert(call_id.to_string(), now);
                true
            }
        }
    }

    /// Forget every call, so a new turn starts unthrottled and the map holds
    /// only the calls of the turn now running.
    pub(super) fn reset(&mut self) {
        self.last_sent.clear();
    }

    /// Replace the interval. A test sets it explicitly so a burst can be held
    /// inside one window without depending on wall-clock timing.
    #[cfg(test)]
    pub(super) fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }
}

#[cfg(test)]
mod tests;
