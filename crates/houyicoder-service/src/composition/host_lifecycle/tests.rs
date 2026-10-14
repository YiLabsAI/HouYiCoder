//! Tests for this module.

use super::*;
use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::oneshot::channel;

/// A task that exits when its token cancels, used to stand in for the
/// drain and relay without pulling their engine collaborators in.
fn token_task(token: CancellationToken, done: Arc<AtomicBool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            _ = token.cancelled() => done.store(true, Ordering::SeqCst),
            _ = pending::<()>() => {}
        }
    })
}

/// begin_shutdown cancels the token the tracked task selects on, so the
/// task exits and the joined future observes its completion. This is the
/// cancel-then-wait contract; the abort fallback covers a task that
/// ignores its token.
#[tokio::test]
async fn test_begin_shutdown_cancels_token() {
    let mut host = HostLifecycle::new();
    let done = Arc::new(AtomicBool::new(false));
    host.track(token_task(host.cancel_token(), Arc::clone(&done)));
    let report = host.begin_shutdown().await;
    assert!(done.load(Ordering::SeqCst), "cancel reached the task");
    assert_eq!(report.panic_count(), 0, "no task panicked");
}

/// Repeated shutdown + install cycles (the in-process resume shape) each
/// reap their own tasks. A leak here would leave old tasks looping across
/// switches; the test asserts each switch's task exits on its own token
/// and the report records a clean stop.
#[tokio::test]
async fn test_begin_shutdown_reaps_repeatedly() {
    for _ in 0..3 {
        let mut host = HostLifecycle::new();
        let done = Arc::new(AtomicBool::new(false));
        host.track(token_task(host.cancel_token(), Arc::clone(&done)));
        let report = host.begin_shutdown().await;
        assert!(done.load(Ordering::SeqCst), "each switch reaps its task");
        assert_eq!(report.panic_count(), 0, "no task panicked");
    }
}

/// begin_shutdown joins a task that exits on its own gate, so the future
/// completes only after the task actually finishes — not when the cancel
/// is called. A caller that treats the cancel as completion would return
/// before done is set; awaiting begin_shutdown must see done true and a
/// Joined outcome.
#[tokio::test]
async fn test_begin_shutdown_joins() {
    let mut host = HostLifecycle::new();
    let (gate_tx, gate_rx) = channel::<()>();
    let done = Arc::new(AtomicBool::new(false));
    let done_c = Arc::clone(&done);
    host.track(tokio::spawn(async move {
        let _ = gate_rx.await;
        done_c.store(true, Ordering::SeqCst);
    }));
    let shutdown = host.begin_shutdown();
    drop(gate_tx);
    let report = shutdown.await;
    assert!(
        done.load(Ordering::SeqCst),
        "begin_shutdown awaited the task"
    );
    assert!(
        matches!(report.outcomes()[0], TaskOutcome::Joined),
        "cooperative task recorded as joined"
    );
}

/// A task that ignores the cancel token and blocks forever cannot exit on
/// its own. begin_shutdown must abort it after the bounded wait and then
/// await the abort so the caller observes actual completion — the cancel
/// call is not join evidence. A guard dropped when the task's future is
/// cancelled records that the abort actually happened, and the report
/// records the Aborted outcome.
#[tokio::test]
async fn test_begin_shutdown_aborts() {
    struct AbortMark {
        flag: Arc<AtomicBool>,
    }
    impl Drop for AbortMark {
        fn drop(&mut self) {
            self.flag.store(true, Ordering::SeqCst);
        }
    }

    let mut host = HostLifecycle::new();
    let aborted = Arc::new(AtomicBool::new(false));
    let mark = AbortMark {
        flag: Arc::clone(&aborted),
    };
    host.track(tokio::spawn(async move {
        let _mark = mark;
        pending::<()>().await;
    }));

    let report = host.begin_shutdown().await;

    assert!(
        aborted.load(Ordering::SeqCst),
        "stuck task was aborted, not abandoned"
    );
    assert!(
        matches!(report.outcomes()[0], TaskOutcome::Aborted),
        "stuck task recorded as aborted"
    );
}

/// A task that panics is recorded as Panicked, not Joined, so the owner
/// reports the failure instead of treating void as success. The global
/// panic hook stays untouched: tokio captures the panic into the
/// JoinError the report reads, and swapping the hook from a panicking
/// thread re-panics during unwinding.
#[tokio::test]
async fn test_begin_shutdown_records_panic() {
    let mut host = HostLifecycle::new();
    host.track(tokio::spawn(async move {
        panic!("writer failed");
    }));
    let report = host.begin_shutdown().await;
    assert_eq!(report.panic_count(), 1, "panic counted as abnormal");
    assert!(
        matches!(report.outcomes()[0], TaskOutcome::Panicked(_)),
        "panic recorded, not swallowed"
    );
}
