//! One driving call at a time on a runner.
//!
//! A model run, a resume, and a user shell command all write the same cancel
//! slot and the same transcript. A second entry while one is in flight would
//! replace the cancel slot, so an abort issued afterwards would never reach
//! the call that is actually running. The slot is claimed by compare-and-set
//! and released by drop, so every exit path releases it.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crate::agent::Runner;

/// A claimed driving slot, released when dropped.
pub(crate) struct DriveGuard<'a> {
    active: &'a AtomicBool,
}

impl Drop for DriveGuard<'_> {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

impl Runner {
    /// Claim the runner's driving slot, or report that another call holds it.
    /// The caller keeps the guard alive for the whole call: the slot is held
    /// from the first write of the transcript to the last.
    pub(crate) fn begin_drive(&self) -> Option<DriveGuard<'_>> {
        self.drive_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| DriveGuard {
                active: &self.drive_active,
            })
    }
}

#[cfg(test)]
mod tests;
