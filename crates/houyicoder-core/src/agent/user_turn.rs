//! The facts one user turn accumulates while it runs: the model calls it has
//! spent against its max_turns budget, and the time its drive legs ran. The
//! turn's closing record reads the work, so a turn that paused for an approval
//! reports the agent's working time rather than the last leg's alone.
//!
//! The legs sum across a pause because the pause is not work: the gap between
//! two legs is the time the user spent deciding, and counting it would make a
//! turn that waited an hour read as an hour of thinking.

use std::time::Duration;

/// The per-user-turn facts. The runner holds one behind a lock, so a turn's
/// budget and the time it spent are read and written as one piece of state.
#[derive(Default)]
pub(super) struct UserTurn {
    calls: u32,
    /// The turn's drive legs added up. None until a leg of this turn ends, so
    /// a turn nobody measured reports no duration rather than a claimed zero.
    worked: Option<Duration>,
}

impl UserTurn {
    /// Spend one model call and return the turn's new count.
    pub(super) fn spend_call(&mut self) -> u32 {
        self.calls += 1;
        self.calls
    }

    /// The model calls this turn has spent.
    pub(super) fn calls(&self) -> u32 {
        self.calls
    }

    /// Start a fresh turn: no calls spent, no work measured.
    pub(super) fn begin(&mut self) {
        self.calls = 0;
        self.worked = None;
    }

    /// Add a finished drive leg's duration to the turn's work.
    pub(super) fn account(&mut self, leg: Duration) {
        self.worked = Some(self.worked.unwrap_or_default() + leg);
    }

    /// Take the turn's measured work, leaving the turn unmeasured again.
    pub(super) fn take_worked(&mut self) -> Option<Duration> {
        self.worked.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calls_reset_on_begin() {
        let mut turn = UserTurn::default();
        assert_eq!(turn.spend_call(), 1);
        assert_eq!(turn.spend_call(), 2);
        assert_eq!(turn.calls(), 2);
        turn.begin();
        assert_eq!(turn.calls(), 0, "a new turn starts with no calls spent");
    }

    /// The legs sum, so the leg that finishes a turn reports what the turn
    /// spent rather than what that leg alone took.
    #[test]
    fn test_worked_sums_across_legs() {
        let mut turn = UserTurn::default();
        assert_eq!(turn.take_worked(), None, "no leg ran, so work is unknown");
        turn.account(Duration::from_secs(2));
        turn.account(Duration::from_secs(3));
        assert_eq!(turn.take_worked(), Some(Duration::from_secs(5)));
        assert_eq!(turn.take_worked(), None, "taking the work leaves none");
        turn.account(Duration::from_secs(7));
        turn.begin();
        assert_eq!(turn.take_worked(), None, "a new turn starts unmeasured");
    }
}
