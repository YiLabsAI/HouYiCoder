//! Tests for this module.

use super::*;
use tokio::sync::mpsc::unbounded_channel;

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
        yield_now().await;
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
    let (tx, mut rx) = unbounded_channel();
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

/// The gate does not evict a wait it admitted: with cap=1, two queued
/// spawns both run as slots free, and overflow stays rejected.
#[tokio::test]
async fn test_queued_not_evicted() {
    let gate = Arc::new(ConcurrencyGate::new(1, 2));
    let p1 = acquired(&gate).await;
    let (tx, mut rx) = unbounded_channel();
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

/// A wait that ends without a slot returns its queue slot. The caller's
/// future is dropped when the spawn call is dropped (a parent stop drops
/// the dispatcher's call), so a cut-short wait must not leave the queue
/// looking full — otherwise repeated stops consume the queue bound and
/// every later spawn is refused.
#[tokio::test]
async fn test_dropped_wait_returns_slot() {
    let gate = Arc::new(ConcurrencyGate::new(1, 1));
    let p1 = acquired(&gate).await;
    let g = Arc::clone(&gate);
    let parked = tokio::spawn(async move { g.acquire().await });
    wait_queued(&gate, 1).await;
    parked.abort();
    drop(parked.await);
    assert_eq!(
        gate.queued_count(),
        0,
        "an ended wait leaves no queue slot behind"
    );
    // The bound is usable again: a fresh wait takes the slot the ended
    // wait gave back and runs when the running slot frees.
    let g = Arc::clone(&gate);
    let (tx, mut rx) = unbounded_channel();
    tokio::spawn(async move {
        let _send = tx.send(g.acquire().await);
    });
    wait_queued(&gate, 1).await;
    drop(p1);
    let r = rx.recv().await.expect("the next wait still gets in");
    assert!(matches!(r, AcquireResult::Acquired(_)));
}

/// try_acquire is non-blocking + never queues: under cap it Acquired, at
/// cap Rejected with no queue growth. Pins the async-spawn contract (a
/// background spawn must not freeze the parent turn waiting for a slot).
#[test]
fn test_try_acquire_rejects() {
    let gate = ConcurrencyGate::new(1, 5);
    let _p1 = gate.try_acquire();
    assert_eq!(gate.queued_count(), 0, "try_acquire does not queue");
    assert!(
        matches!(gate.try_acquire(), AcquireResult::Rejected),
        "at cap, try_acquire rejects immediately without waiting"
    );
    assert_eq!(
        gate.queued_count(),
        0,
        "rejected try_acquire leaves no queue"
    );
}
