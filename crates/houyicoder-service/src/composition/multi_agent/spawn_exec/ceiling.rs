//! The wall-clock ceiling a delegation runs under, and how a run that hits it
//! is reported.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use houyicoder_async::CancellationToken;
use houyicoder_core::agent::multi_agent::CutReason;
use houyicoder_protocol::llm::Usage;
use tokio::time::timeout;

/// Await a child's drive under its ceiling.
///
/// Dropping the drive at the deadline would dismantle the child before its
/// tail ran: that tail publishes the child's terminal, closes its inbox, and
/// releases the fence slot, so the parent would lose both the cleanup and
/// whatever the child had already finished. The deadline therefore stops the
/// child through the same cancellation a shutdown uses and keeps awaiting,
/// which yields the ending the cancelled run reconciles.
///
/// The reason is recorded in the shared slot before the cancel, not after: the
/// cancel provokes the child's own terminal publish, and that publish reads the
/// slot to name the ending. Writing it afterwards would race the very message
/// it has to explain, and the ending would report a plain interrupt.
pub(super) async fn run_with_deadline(
    drive: impl std::future::Future<Output = (String, String, Usage)>,
    timeout_secs: Option<u64>,
    cancel: CancellationToken,
    cut_reason: Arc<Mutex<Option<CutReason>>>,
) -> (String, String, Usage) {
    let mut drive = Box::pin(drive);
    let Some(secs) = timeout_secs else {
        return drive.await;
    };
    match timeout(Duration::from_secs(secs), &mut drive).await {
        Ok(done) => done,
        Err(_) => {
            match cut_reason.lock() {
                Ok(mut slot) => *slot = Some(CutReason::TimedOut { secs, stage: None }),
                Err(_) => tracing::warn!(
                    "the cut reason slot is poisoned; the timed-out child will report as an interrupt"
                ),
            }
            cancel.cancel();
            drive.await
        }
    }
}
