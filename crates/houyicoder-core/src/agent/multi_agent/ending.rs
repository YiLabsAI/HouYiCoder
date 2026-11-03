//! Which side receives a child's ending: the call that waits on it, or the
//! parent's notification stream. Both sides decide in different tasks, so the
//! decision is one atomic claim; the winner fixes the run mode the ending is
//! published with and the loser adapts.

use std::sync::atomic::{AtomicU8, Ordering};

use super::bus_types::ChildRunMode;

const UNDECIDED: u8 = 0;
const CALLER: u8 = 1;
const NOTIFY: u8 = 2;

/// One child's ending, claimed by exactly one side.
///
/// A child started in the background notifies from the start; a foreground
/// child starts undecided, and either its publisher claims the ending for the
/// waiting caller or a hand-over claims it for the notification stream.
pub struct EndingClaim {
    state: AtomicU8,
}

impl EndingClaim {
    /// Start claimed for the notification stream when the child was spawned in
    /// the background, or undecided when a caller waits on it.
    pub fn new(run_mode: ChildRunMode) -> Self {
        let state = match run_mode {
            ChildRunMode::Background => NOTIFY,
            ChildRunMode::Foreground => UNDECIDED,
        };
        Self {
            state: AtomicU8::new(state),
        }
    }

    /// Claim the ending for the call that waits on the child. False means the
    /// child was already handed to the background, so that caller has no
    /// terminal result. A repeat claim by the same side reports true.
    pub fn claim_caller(&self) -> bool {
        match self
            .state
            .compare_exchange(UNDECIDED, CALLER, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => true,
            Err(state) => state == CALLER,
        }
    }

    /// Hand the ending to the notification stream. True only for the caller
    /// that changed the decision.
    pub fn claim_notification(&self) -> bool {
        self.state
            .compare_exchange(UNDECIDED, NOTIFY, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Whether the ending belongs to the parent's notification stream.
    pub fn notifies_parent(&self) -> bool {
        self.state.load(Ordering::Acquire) == NOTIFY
    }
}

#[cfg(test)]
mod tests;
