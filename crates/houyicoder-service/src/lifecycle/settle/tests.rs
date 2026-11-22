use std::future::{pending, ready};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::settle_on_disconnect;

/// Work that is already ready settles without aborting: the first poll
/// resolves it, so the cancel never fires and the budget does not run.
#[tokio::test]
async fn test_settle_ready_skips_abort() {
    let aborted = Arc::new(AtomicBool::new(false));
    let flag = aborted.clone();
    let work = ready(42);
    tokio::pin!(work);
    settle_on_disconnect(&mut work, Duration::from_millis(50), move || {
        flag.store(true, Ordering::SeqCst)
    })
    .await;
    assert!(
        !aborted.load(Ordering::SeqCst),
        "ready work does not need an abort"
    );
}

/// Work that is still pending triggers the abort and waits out the budget
/// when nothing resolves it, so a stuck tool does not pin the host forever.
#[tokio::test]
async fn test_settle_pending_aborts() {
    let aborted = Arc::new(AtomicBool::new(false));
    let flag = aborted.clone();
    let work = pending::<()>();
    tokio::pin!(work);
    settle_on_disconnect(&mut work, Duration::from_millis(50), move || {
        flag.store(true, Ordering::SeqCst)
    })
    .await;
    assert!(
        aborted.load(Ordering::SeqCst),
        "pending work triggers the abort"
    );
}
