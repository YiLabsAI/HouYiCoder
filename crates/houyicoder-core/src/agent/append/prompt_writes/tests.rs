//! Tests for this module.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::yield_now;
use tokio::time::timeout;

use super::PendingPromptWrites;

#[tokio::test]
async fn test_wait_returns_without_writes() {
    let pending = PendingPromptWrites::default();
    pending.wait().await;
}

#[tokio::test]
async fn test_wait_blocks_while_writing() {
    let pending = Arc::new(PendingPromptWrites::default());
    let ticket = pending.begin();
    let tracker = Arc::clone(&pending);
    let waiter = tokio::spawn(async move { tracker.wait().await });
    for _ in 0..8 {
        yield_now().await;
    }
    assert!(
        !waiter.is_finished(),
        "a write in progress keeps the waiter waiting"
    );
    drop(ticket);
    // A lost wake hangs the waiter, so the wait is bounded: the failure reads
    // as a timeout instead of a test that never finishes.
    timeout(Duration::from_secs(5), waiter)
        .await
        .expect("the waiter returns once the write ends")
        .expect("the waiter task did not panic");
}
