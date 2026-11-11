//! One wake signal shared by every producer the run loop waits on.
//!
//! Each producer raises the flag after its own queue holds the item, so a
//! burst of arrivals costs one frame. A wake raised before the loop waits is
//! remembered, not lost: the flag is set under the same mutex the wait checks.

use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

/// A pending-wake flag and the waiters that read it.
#[derive(Debug, Default)]
pub(crate) struct UiWake {
    signaled: Mutex<bool>,
    ready: Condvar,
}

impl UiWake {
    /// A wake the caller owns and passes to the loop it drives. Production
    /// uses the process-wide one; a test uses its own so no other test in the
    /// binary shares its flag.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Mark work as pending and release one waiter.
    pub(crate) fn wake(&self) {
        let mut signaled = self.signaled.lock().unwrap_or_else(|e| e.into_inner());
        *signaled = true;
        self.ready.notify_one();
    }

    /// Block until woken or until the timeout expires, consuming the flag.
    /// Returns whether a wake was raised, so a caller can tell a producer's
    /// arrival from a timer expiring.
    pub(crate) fn wait(&self, timeout: Option<Duration>) -> bool {
        let mut signaled = self.signaled.lock().unwrap_or_else(|e| e.into_inner());
        if !*signaled {
            signaled = match timeout {
                Some(timeout) => {
                    self.ready
                        .wait_timeout(signaled, timeout)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
                None => self.ready.wait(signaled).unwrap_or_else(|e| e.into_inner()),
            };
        }
        let woken = *signaled;
        *signaled = false;
        woken
    }
}

/// The process-wide wake. A session switch runs the run loop again against
/// the same long-lived producers, so every run waits on the one flag those
/// producers raise.
static UI_WAKE: OnceLock<Arc<UiWake>> = OnceLock::new();

/// The wake every long-lived producer raises.
pub(crate) fn ui_wake() -> &'static Arc<UiWake> {
    UI_WAKE.get_or_init(UiWake::new)
}

#[cfg(test)]
mod tests;
