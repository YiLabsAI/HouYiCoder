//! Prompt writes that are still running.
//!
//! A turn reaches its first model call without waiting for the descriptor
//! write its input starts, so the write can outlive the append that caused
//! it. A caller that reads the descriptor right after such an append waits
//! here first, and a test asserting that nothing was recorded needs the same
//! wait: without it, an unfinished write and a write that never happened look
//! identical.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::Notify;

/// Counts the prompt writes in progress so a reader can wait for a settled
/// descriptor instead of guessing how long a write takes.
#[derive(Default)]
pub(crate) struct PendingPromptWrites {
    running: AtomicUsize,
    idle: Notify,
}

impl PendingPromptWrites {
    /// Mark one write as started. The returned ticket drops the count when
    /// the write ends, so a store that fails or panics still returns the count
    /// to zero instead of leaving a waiter asleep forever.
    pub(crate) fn begin(self: &Arc<Self>) -> WriteTicket {
        self.running.fetch_add(1, Ordering::AcqRel);
        WriteTicket(Arc::clone(self))
    }

    /// Wait until no prompt write is running. The future is created before the
    /// count is read, and the notify primitive reaches a waiter from the
    /// moment its future exists, even before it is first polled, so a write
    /// that finishes in between still wakes this waiter instead of leaving it
    /// asleep on an idle count.
    pub(crate) async fn wait(&self) {
        loop {
            let notified = self.idle.notified();
            if self.running.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// One prompt write in progress, held for the write's whole lifetime.
pub(crate) struct WriteTicket(Arc<PendingPromptWrites>);

impl Drop for WriteTicket {
    fn drop(&mut self) {
        if self.0.running.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests;
