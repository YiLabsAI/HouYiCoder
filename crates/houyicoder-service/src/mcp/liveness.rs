//! Restart policy for a server whose connection drops: how many times it is
//! restarted and how long each wait lasts.

use std::time::Duration;

/// The bounded restart policy: a base wait that doubles per attempt, capped,
/// and a ceiling on attempts past which the server is reported failed rather
/// than restarted forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartPolicy {
    base: Duration,
    max: Duration,
    attempts: u32,
}

impl RestartPolicy {
    /// A policy with a base wait, a cap, and an attempt ceiling.
    pub fn new(base: Duration, max: Duration, attempts: u32) -> Self {
        Self {
            base,
            max,
            attempts,
        }
    }

    /// How long to wait before the given attempt, or None once the ceiling
    /// is reached. The first attempt waits the base; each later one doubles
    /// it until the cap.
    pub fn delay(&self, attempt: u32) -> Option<Duration> {
        if attempt == 0 || attempt > self.attempts {
            return None;
        }
        let doublings = attempt - 1;
        let scaled = self
            .base
            .checked_mul(1u32.checked_shl(doublings).unwrap_or(u32::MAX))
            .unwrap_or(self.max);
        Some(scaled.min(self.max))
    }
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self::new(Duration::from_secs(1), Duration::from_secs(30), 3)
    }
}

#[cfg(test)]
mod tests;
