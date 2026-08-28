//! Concurrency cap and bounded queue for sub-agent spawns.
//!
//! At most cap children run at once; up to queue_cap more wait and run when
//! a slot frees; beyond that a spawn is rejected with backpressure. A queued
//! spawn is never dropped: once admitted it runs when a slot frees; only
//! overflow is refused, and that refusal is explicit. The token budget gate
//! (budget.rs) guards total token spend; this gate guards concurrent
//! resource use -- memory, in-flight API requests, worktree fence slots.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The verdict from a concurrency-gate acquire.
pub enum AcquireResult {
    /// A running slot was granted. Dropping the permit releases the slot so
    /// a queued spawn can proceed.
    Acquired(OwnedSemaphorePermit),
    /// The running slots are full and the queue is saturated; the spawn is
    /// rejected with backpressure. Not a drop: a queued request is never
    /// evicted once admitted, only overflow is refused.
    Rejected,
}

/// A per-parent concurrency gate. At most cap children run at once; up to
/// queue_cap more can wait; beyond that, acquires are rejected. The queue
/// bound is hard under contention: a compare-and-swap claims each queue slot
/// atomically, so the waiter count never exceeds queue_cap. The no-drop
/// invariant holds unconditionally: the blocking acquire only returns once a
/// slot is held, and the permit releases on drop into the next waiter.
pub struct ConcurrencyGate {
    running: Arc<Semaphore>,
    queued: AtomicUsize,
    queue_cap: usize,
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

    /// The default queue bound. A queued spawn is never dropped; overflow
    /// beyond cap + queue_cap is rejected with backpressure.
    pub const DEFAULT_QUEUE_CAP: usize = 5;

    /// Acquire a running slot. Fast path: a slot is free, returns Acquired.
    /// Slow path: slots full but the queue has room, blocks until a slot
    /// frees. Reject path: queue saturated, returns Rejected.
    pub async fn acquire(&self) -> AcquireResult {
        if let Ok(permit) = self.running.clone().try_acquire_owned() {
            return AcquireResult::Acquired(permit);
        }
        loop {
            let q = self.queued.load(Ordering::Relaxed);
            if q >= self.queue_cap {
                return AcquireResult::Rejected;
            }
            if self
                .queued
                .compare_exchange(q, q + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
        match self.running.clone().acquire_owned().await {
            Ok(permit) => {
                self.queued.fetch_sub(1, Ordering::Relaxed);
                AcquireResult::Acquired(permit)
            }
            Err(_) => {
                self.queued.fetch_sub(1, Ordering::Relaxed);
                AcquireResult::Rejected
            }
        }
    }

    /// The number of spawns currently waiting for a running slot. Observable
    /// for diagnostics; the no-drop invariant holds regardless of the value.
    pub fn queued_count(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn acquired(gate: &ConcurrencyGate) -> OwnedSemaphorePermit {
        match gate.acquire().await {
            AcquireResult::Acquired(p) => p,
            AcquireResult::Rejected => panic!("expected Acquired, got Rejected"),
        }
    }

    /// Wait until at least n spawns are queued, or panic. Yields between
    /// checks so background tasks enter the queue.
    async fn wait_queued(gate: &ConcurrencyGate, n: usize) {
        for _ in 0..200 {
            if gate.queued_count() >= n {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("only {} queued, expected {n}", gate.queued_count());
    }

    /// The 6th spawn (cap full) queues, not runs immediately, and is not
    /// dropped: when a running slot frees it acquires and runs.
    #[tokio::test]
    async fn test_overflow_queues_not_dropped() {
        let gate = Arc::new(ConcurrencyGate::new(2, 2));
        let p1 = acquired(&gate).await;
        let p2 = acquired(&gate).await;
        let g = Arc::clone(&gate);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let _send = tx.send(g.acquire().await);
        });
        wait_queued(&gate, 1).await;
        drop(p1);
        let r = rx.recv().await.expect("queued spawn ran, not dropped");
        assert!(matches!(r, AcquireResult::Acquired(_)));
        drop(p2);
    }

    /// Beyond cap + queue_cap, the newest acquire is rejected with
    /// backpressure, not silently dropped.
    #[tokio::test]
    async fn test_overflow_rejects_newest() {
        let gate = Arc::new(ConcurrencyGate::new(2, 2));
        let p1 = acquired(&gate).await;
        let p2 = acquired(&gate).await;
        for _ in 0..2 {
            let g = Arc::clone(&gate);
            tokio::spawn(async move {
                let _permit = g.acquire().await;
            });
        }
        wait_queued(&gate, 2).await;
        assert!(
            matches!(gate.acquire().await, AcquireResult::Rejected),
            "5th must be rejected, not queued or dropped"
        );
        assert!(
            matches!(gate.acquire().await, AcquireResult::Rejected),
            "6th must be rejected, not queued or dropped"
        );
        drop(p1);
        drop(p2);
    }

    /// A queued spawn is never evicted: with cap=1, two queued spawns both
    /// run as slots free, and overflow stays rejected.
    #[tokio::test]
    async fn test_queued_not_evicted() {
        let gate = Arc::new(ConcurrencyGate::new(1, 2));
        let p1 = acquired(&gate).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for _ in 0..2 {
            let g = Arc::clone(&gate);
            let tx = tx.clone();
            tokio::spawn(async move {
                let _send = tx.send(g.acquire().await);
            });
        }
        wait_queued(&gate, 2).await;
        assert!(matches!(gate.acquire().await, AcquireResult::Rejected));
        drop(p1);
        let first = rx.recv().await.expect("first queued spawn ran");
        assert!(matches!(first, AcquireResult::Acquired(_)));
        drop(first);
        let second = rx.recv().await.expect("second queued spawn ran");
        assert!(matches!(second, AcquireResult::Acquired(_)));
    }

    /// Fast path: under-cap acquires never touch the queue counter.
    #[tokio::test]
    async fn test_under_cap_no_queue() {
        let gate = ConcurrencyGate::new(3, 3);
        let p1 = acquired(&gate).await;
        let p2 = acquired(&gate).await;
        assert_eq!(gate.queued_count(), 0);
        drop(p1);
        drop(p2);
    }

    /// Zero queue_cap: any overflow rejects immediately.
    #[tokio::test]
    async fn test_zero_queue_rejects_overflow() {
        let gate = ConcurrencyGate::new(1, 0);
        let p1 = acquired(&gate).await;
        assert!(
            matches!(gate.acquire().await, AcquireResult::Rejected),
            "with no queue, overflow rejects immediately"
        );
        drop(p1);
    }
}
