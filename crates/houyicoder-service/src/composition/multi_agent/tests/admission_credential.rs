//! Admission-credential unit tests: the slot model and the bounded wait
//! join, driven directly against the private registry so the shutdown
//! contract is pinned without a full runtime.

use std::sync::Arc;
use std::time::Duration;

use houyicoder_async::CancellationToken;
use tokio::sync::Notify;
use tokio::sync::oneshot::channel;
use tokio::sync::oneshot::error::TryRecvError;

use super::super::{ChildCredential, ChildWatch, RunningChildren, join_child};
use std::future::pending;
use tokio::time::timeout;

/// The foreground join confirms the task's end signal closed, not merely
/// that it issued the abort: a done Sender the abort cannot reach keeps the
/// join parked even after the timeout backstop fires, and only releasing it
/// lets the join return. The old shape resolved on abort alone; the guard's
/// signal proves the backstop actually ran before this test checks the join,
/// so it waits past the abort rather than passing on a join that never entered
/// the timeout.
#[tokio::test]
async fn test_join_foreground_confirms_end() {
    let (done_tx, done) = channel::<()>();
    let aborted = Arc::new(Notify::new());
    // A task borrowed only so the awaited entry has an AbortHandle to fire.
    // Its body is pending, so the abort backstop is the only way it ends; the
    // guard signals the instant the task is dropped, which is the moment the
    // backstop ran.
    let abort = {
        let aborted = Arc::clone(&aborted);
        tokio::spawn(async move {
            struct SignalOnDrop(Arc<Notify>);
            impl Drop for SignalOnDrop {
                fn drop(&mut self) {
                    self.0.notify_one();
                }
            }
            let _signal = SignalOnDrop(aborted);
            pending::<()>().await
        })
        .abort_handle()
    };
    let slot = ChildCredential {
        close: CancellationToken::new(),
        finished: done,
        watch: Some(ChildWatch::Awaited { abort }),
    };
    let join = tokio::spawn(join_child(slot, Duration::from_millis(10)));
    // Wait until the backstop fired, not merely until the join task was
    // spawned: on the current-thread runtime a freshly spawned task has not run
    // yet, so is_finished alone would be trivially false.
    timeout(Duration::from_secs(1), aborted.notified())
        .await
        .expect("the join's timeout fires the abort backstop");
    // The abort has run, but done is still open: the join must still be parked.
    assert!(
        !join.is_finished(),
        "the join stays parked on the open done signal, even after the abort fired"
    );
    drop(done_tx);
    join.await.expect("the join resolves once done closes");
}

/// Reserve creates the admitted slot's credential before any child exists: a
/// shutdown landing between admission and attach still cancels the close and
/// still carries the slot into the drained set, and a later admission is refused
/// without discarding an unconsumed drain.
#[tokio::test]
async fn test_admission_enters_drain() {
    let mut registry = RunningChildren::default();
    let admission = registry
        .admit(CancellationToken::new())
        .expect("an open gate admits");
    let slot = registry
        .children
        .get_mut(&admission.slot)
        .expect("slot present");
    assert!(
        slot.watch.is_none(),
        "admitting registers the credential before any task attaches"
    );
    assert!(
        matches!(slot.finished.try_recv(), Err(TryRecvError::Empty)),
        "the completion receiver is live from the admission, before the task takes its sender"
    );
    // The close entrance the join uses: cancel every admitted close under the
    // lock, then close the gate and drain.
    for s in registry.children.values() {
        s.close.cancel();
    }
    assert!(
        registry
            .children
            .get(&admission.slot)
            .unwrap()
            .close
            .is_cancelled(),
        "the admitted credential's close reaches the admitted child"
    );
    registry.close_and_drain();
    // A repeated close before the drain is consumed must not discard the
    // first: the single-close consumer stays the boundary.
    registry.close_and_drain();
    let mut drained = registry.take_drained();
    assert_eq!(
        drained.len(),
        1,
        "the admitted slot survives the repeated close"
    );
    assert!(drained[0].1.close.is_cancelled());
    assert!(
        matches!(drained[0].1.finished.try_recv(), Err(TryRecvError::Empty)),
        "the unwatched slot's completion still waits for its never-built task"
    );
    // Once closed, a new admission is refused.
    assert!(
        registry.admit(CancellationToken::new()).is_none(),
        "a closed gate admits nothing"
    );
}
