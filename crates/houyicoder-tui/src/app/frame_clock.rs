//! Frame pacing for the event-driven run loop.
//!
//! Producers wake the loop on every arrival, and a paste or a token burst
//! delivers arrivals far faster than a terminal can repaint. The clock holds
//! frames to a fixed minimum spacing so a burst merges into one frame. The
//! first frame after silence draws at once, because no recent draw sets a
//! lower bound.

use std::time::{Duration, Instant};

/// The spacing that caps repaints at 120 frames per second.
pub(crate) const FRAME_INTERVAL: Duration = Duration::from_nanos(8_333_333);

/// The minimum spacing between two drawn frames.
#[derive(Debug)]
pub(crate) struct FrameClock {
    interval: Duration,
    last: Option<Instant>,
}

impl Default for FrameClock {
    fn default() -> Self {
        Self::new(FRAME_INTERVAL)
    }
}

impl FrameClock {
    /// A clock with the given spacing, for a test that must not wait out a
    /// real frame interval.
    pub(crate) fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
        }
    }

    /// How long the next frame must wait, or None when it may draw now.
    pub(crate) fn cooldown(&self, now: Instant) -> Option<Duration> {
        let last = self.last?;
        let elapsed = now.saturating_duration_since(last);
        if elapsed < self.interval {
            Some(self.interval - elapsed)
        } else {
            None
        }
    }

    /// Record that a frame was drawn at the given instant.
    pub(crate) fn note_frame(&mut self, now: Instant) {
        self.last = Some(now);
    }
}

#[cfg(test)]
mod tests;
