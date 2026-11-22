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
pub(crate) struct UserTurn {
    calls: u32,
    /// The turn's drive legs added up. None until a leg of this turn ends, so
    /// a turn nobody measured reports no duration rather than a claimed zero.
    worked: Option<Duration>,
    /// Set when a budget watchdog asks this run to stop calling tools and emit
    /// one final text summary before termination.
    wrapping_up: bool,
    /// Set when wrapping up was triggered by reaching the final turn of the
    /// turn budget rather than an external timer.
    turn_limit_wrapped: bool,
    /// Grants one turn beyond max_turns when an external watchdog interrupts
    /// a run that has already spent its final turn.
    extra_wrap_turn: bool,
    /// Pending internal directive written as MetaUser at the next turn
    /// boundary so wrap-up does not overwrite session intent or render as user
    /// input.
    wrap_up_notice: Option<String>,
}

impl UserTurn {
    /// Spend one model call and return the turn's new count.
    pub(crate) fn spend_call(&mut self) -> u32 {
        self.calls += 1;
        self.calls
    }

    /// The model calls this turn has spent.
    pub(crate) fn calls(&self) -> u32 {
        self.calls
    }

    /// Start a fresh turn: no calls spent, no work measured, not wrapping up.
    pub(crate) fn begin(&mut self) {
        self.calls = 0;
        self.worked = None;
        self.wrapping_up = false;
        self.turn_limit_wrapped = false;
        self.extra_wrap_turn = false;
        self.wrap_up_notice = None;
    }

    /// Enter final wrap-up mode, returning true on the first transition.
    pub(crate) fn begin_wrap_up(&mut self, directive: String, grant_extra_turn: bool) -> bool {
        let first = !self.wrapping_up;
        self.wrapping_up = true;
        if first {
            self.extra_wrap_turn = grant_extra_turn;
            self.turn_limit_wrapped = !grant_extra_turn;
            self.wrap_up_notice = Some(directive);
        }
        first
    }

    /// Whether wrapping up was initiated by the turn-budget ceiling.
    pub(crate) fn turn_limit_wrapped(&self) -> bool {
        self.turn_limit_wrapped
    }

    /// Whether an external watchdog granted one extra turn past max_turns.
    pub(crate) fn extra_wrap_turn(&self) -> bool {
        self.extra_wrap_turn
    }

    /// Take any pending wrap-up directive for injection as a MetaUser event.
    pub(crate) fn take_wrap_up_notice(&mut self) -> Option<String> {
        self.wrap_up_notice.take()
    }

    /// Whether this run has entered final wrap-up mode.
    pub(crate) fn is_wrapping_up(&self) -> bool {
        self.wrapping_up
    }

    /// Add a finished drive leg's duration to the turn's work.
    pub(crate) fn account(&mut self, leg: Duration) {
        self.worked = Some(self.worked.unwrap_or_default() + leg);
    }

    /// Take the turn's measured work, leaving the turn unmeasured again.
    pub(crate) fn take_worked(&mut self) -> Option<Duration> {
        self.worked.take()
    }
}

#[cfg(test)]
mod tests;
