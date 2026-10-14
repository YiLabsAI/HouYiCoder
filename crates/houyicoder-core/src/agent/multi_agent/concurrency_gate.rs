//! Concurrency cap and bounded queue for sub-agent spawns.
//!
//! At most cap children run at once; up to queue_cap more wait and run when
//! a slot frees; beyond that a spawn is rejected with backpressure. The gate
//! does not evict a wait it admitted: the wait runs when a slot frees unless
//! its own caller goes away, in which case it returns its queue slot, so a
//! cut-short wait does not consume the bound. Only overflow is refused, and
//! that refusal is explicit. The token budget gate (budget.rs) guards total
//! token spend; this gate guards concurrent resource use -- memory, live API
//! requests, worktree fence slots.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
#[cfg(test)]
use tokio::task::yield_now;

/// The verdict from a concurrency-gate acquire.
pub enum AcquireResult {
    /// A running slot was granted. Dropping the permit releases the slot so
    /// a queued spawn can proceed.
    Acquired(OwnedSemaphorePermit),
    /// The running slots are full and the queue is saturated; the spawn is
    /// rejected with backpressure. The gate does not evict a wait it already
    /// admitted: that wait runs when a slot frees.
    Rejected,
}

/// A per-parent concurrency gate. At most cap children run at once; up to
/// queue_cap more can wait; beyond that, acquires are rejected. The queue
/// bound is hard under contention: a compare-and-swap takes each queue slot
/// atomically, so the waiter count never exceeds queue_cap. The count holds
/// only waits that have not returned their slot: a wait that takes a running
/// slot, is refused, or has its future dropped gives the slot back, and the
/// permit releases on drop into the next waiter.
pub struct ConcurrencyGate {
    running: Arc<Semaphore>,
    queued: AtomicUsize,
    queue_cap: usize,
}

/// One queue slot held for a parked wait, returned to the queue on drop. Held
/// by the waiting acquire, so every way a wait can end — a slot, a refusal, or
/// a dropped caller future — gives the slot back.
struct QueueSlot<'a> {
    queued: &'a AtomicUsize,
}

impl Drop for QueueSlot<'_> {
    fn drop(&mut self) {
        // The counter is an admission bound: atomicity is all it needs, since
        // the semaphore, not this count, carries the synchronization between
        // a slot freeing and the waiter that takes it.
        self.queued.fetch_sub(1, Ordering::Relaxed);
    }
}

impl ConcurrencyGate {
    /// Create a gate with cap concurrent running slots and a queue_cap wait
    /// pool. Beyond cap + queue_cap, acquires are rejected.
    pub fn new(cap: usize, queue_cap: usize) -> Self {
        Self {
            running: Arc::new(Semaphore::new(cap)),
            queued: AtomicUsize::new(0),
            queue_cap,
        }
    }

    /// The default concurrent-running cap (fanout default 5; industry
    /// experience: more than 5 coordination overhead exceeds benefit).
    pub const DEFAULT_CAP: usize = 5;

    /// The default queue bound. A wait already admitted is not evicted;
    /// overflow beyond cap + queue_cap is rejected with backpressure.
    pub const DEFAULT_QUEUE_CAP: usize = 5;

    /// Acquire a running slot. Fast path: a slot is free, returns Acquired.
    /// Slow path: slots full but the queue has room, blocks until a slot
    /// frees. Reject path: queue saturated, returns Rejected. The foreground spawn
    /// path uses this (interactive, blocking — a spawn waits for a slot up to
    /// the queue bound rather than refusing the user's request at the first
    /// full cap). A wait cut short — its caller's future is dropped — returns
    /// its queue slot instead of leaving the queue fuller than it is.
    pub async fn acquire(&self) -> AcquireResult {
        if let Ok(permit) = self.running.clone().try_acquire_owned() {
            return AcquireResult::Acquired(permit);
        }
        let Some(slot) = self.take_queue_slot() else {
            return AcquireResult::Rejected;
        };
        match self.running.clone().acquire_owned().await {
            Ok(permit) => {
                drop(slot);
                AcquireResult::Acquired(permit)
            }
            // Nothing closes this semaphore; a closed one would mean the gate
            // is gone rather than saturated, and either way there is no slot
            // to hand out.
            Err(_) => AcquireResult::Rejected,
        }
    }

    /// Take one queue slot, or None when the queue is saturated. The slot is a
    /// guard: it returns itself on drop, so the count falls back on every path
    /// that ends the wait.
    fn take_queue_slot(&self) -> Option<QueueSlot<'_>> {
        loop {
            let q = self.queued.load(Ordering::Relaxed);
            if q >= self.queue_cap {
                return None;
            }
            if self
                .queued
                .compare_exchange(q, q + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Some(QueueSlot {
                    queued: &self.queued,
                });
            }
        }
    }

    /// Non-blocking acquire: take a free slot if one is open, else Reject
    /// immediately — no queue, no wait. The background spawn path uses this so a
    /// background spawn never blocks the parent turn: a spawn that finds the
    /// cap full rejects with ConcurrencySaturated and the model re-queues
    /// next turn, rather than freezing the parent until a child completes.
    /// The queue is sync-path only (interactive spawns wait; background
    /// spawns refuse and retry).
    pub fn try_acquire(&self) -> AcquireResult {
        match self.running.clone().try_acquire_owned() {
            Ok(permit) => AcquireResult::Acquired(permit),
            Err(_) => AcquireResult::Rejected,
        }
    }

    /// The number of waits currently parked for a running slot, for a caller
    /// that wants to report or assert queue depth.
    pub fn queued_count(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests;
