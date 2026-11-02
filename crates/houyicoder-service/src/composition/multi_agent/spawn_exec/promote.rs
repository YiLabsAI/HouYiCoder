//! Waiting on a foreground child while the reader may promote it.
//!
//! A foreground delegation holds the parent's turn open until the child ends.
//! The reader can hand that child to the background instead, so the wait is a
//! race between the child's own outcome and the promotion signal. The child
//! keeps running either way; only who waits for it changes.

use houyicoder_api::spawn::SpawnOutcome;

use super::SpawnFailure;
use std::panic::resume_unwind;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Wait for the child's outcome, or return the launch as soon as the reader
/// promotes it.
///
/// The child's own outcome is checked first: a promotion that lands in the
/// same instant the child finishes must not report a launch for work that is
/// already done, because the parent would then never see the answer it already
/// paid for. Once promoted, the driver keeps running on its own and the child's
/// ending reaches the parent as a notification, which is what the promotion
/// flag the runtime flipped is for.
pub(super) async fn await_child_or_promotion(
    driver: &mut JoinHandle<Result<SpawnOutcome, SpawnFailure>>,
    promote: &Notify,
    child_str: String,
) -> Result<SpawnOutcome, SpawnFailure> {
    tokio::select! {
        biased;
        outcome = &mut *driver => match outcome {
            Ok(result) => result,
            // A panic inside the driver would otherwise reach the model as an
            // ordinary refusal, so surface it as the panic it is.
            Err(join) => resume_unwind(join.into_panic()),
        },
        _ = promote.notified() => Ok(SpawnOutcome::background_started(child_str)),
    }
}

/// Wait for a driver that never reported a child, surfacing its own result.
pub(super) async fn await_driver(
    driver: JoinHandle<Result<SpawnOutcome, SpawnFailure>>,
) -> Result<SpawnOutcome, SpawnFailure> {
    match driver.await {
        Ok(result) => result,
        Err(join) => resume_unwind(join.into_panic()),
    }
}
