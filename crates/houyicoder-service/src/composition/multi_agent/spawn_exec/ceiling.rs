//! The wall-clock ceiling a delegation runs under, and how a run that hits it
//! is reported.

use houyicoder_async::CancellationToken;
use houyicoder_protocol::llm::Usage;
use std::time::Duration;
use tokio::time::timeout;

/// The status a delegation reports when its own ceiling ended the run. It is
/// distinct from the ending the child publishes: that one reads the same as a
/// user interrupt, and only the caller that set the ceiling knows why.
pub(super) const TIMED_OUT_STATUS: &str = "timed_out";

/// How a delegation ended, from the ceiling's point of view.
pub(super) enum CeilingEnding {
    /// The child reached a terminal of its own within its ceiling.
    Finished((String, String, Usage)),
    /// The ceiling stopped it, and this is what it had produced by then.
    Cut {
        secs: u64,
        done: (String, String, Usage),
    },
}

/// Await a child's drive under its ceiling.
///
/// Dropping the drive at the deadline would dismantle the child before its
/// tail ran: that tail publishes the child's terminal, closes its inbox, and
/// releases the fence slot, so the parent would lose both the cleanup and
/// whatever the child had already finished. The deadline therefore stops the
/// child through the same cancellation a shutdown uses and keeps awaiting,
/// which yields the ending the cancelled run reconciles.
pub(super) async fn await_ceilinged(
    drive: impl std::future::Future<Output = (String, String, Usage)>,
    timeout_secs: Option<u64>,
    cancel: CancellationToken,
) -> CeilingEnding {
    let mut drive = Box::pin(drive);
    let Some(secs) = timeout_secs else {
        return CeilingEnding::Finished(drive.await);
    };
    match timeout(Duration::from_secs(secs), &mut drive).await {
        Ok(done) => CeilingEnding::Finished(done),
        Err(_) => {
            cancel.cancel();
            CeilingEnding::Cut {
                secs,
                done: drive.await,
            }
        }
    }
}
