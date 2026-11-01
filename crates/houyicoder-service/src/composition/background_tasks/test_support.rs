//! Shared waits for tests that drive a launched background task. A wait here
//! yields to the runtime instead of sleeping, so the test pays no fixed delay
//! and a condition that never holds fails the test rather than hanging it.

use tokio::task::yield_now;

/// Yield to the runtime until the condition holds, failing after a bounded
/// number of polls.
pub(crate) async fn wait_until(mut reached: impl FnMut() -> bool) {
    for _ in 0..500 {
        if reached() {
            return;
        }
        yield_now().await;
    }
    panic!("condition was not reached");
}
