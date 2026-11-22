//! Settling an in-flight turn when the client connection drops: arm the
//! cancel slot, abort the runner, and wait within a budget for the paired
//! tool result to land so the durable trail closes cleanly. Shared by the
//! frontend server's driving loop and the ACP server's drive and resume
//! loops, so a disconnect leaves no orphaned tool call that breaks resume.

use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use tokio::time::timeout;

/// The budget a disconnected host waits for a cancelled turn's in-flight tool
/// to append its paired result before giving up. Long enough for a tool that
/// already started its effect to record the outcome; short enough that a
/// stuck backend does not pin the host.
pub(crate) const CLOSE_SETTLE_BUDGET: Duration = Duration::from_secs(5);

/// Settle one in-flight future when the client left. The first poll is where
/// the operation installs the cancel handle it watches, so it must happen
/// before the abort: aborting before that poll reaches the slot the previous
/// operation left (or nothing) leaves the command running uncancelled. Work
/// that finishes on this poll has nothing left to cancel, so the abort is
/// skipped.
///
/// After the abort, wait within the budget for the future to resolve so a
/// tool call that already wrote its ToolCall appends the paired ToolResult
/// the session log expects, instead of leaving an orphan that breaks the next
/// resume. The abort is a closure so the caller owns how the runner cancels.
pub(crate) async fn settle_on_disconnect<F, A>(work: &mut Pin<&mut F>, budget: Duration, abort: A)
where
    F: Future,
    F::Output: Debug,
    A: FnOnce(),
{
    if let Poll::Ready(outcome) = futures::poll!(work.as_mut()) {
        tracing::debug!(?outcome, "work finished as the client left");
        return;
    }
    abort();
    match timeout(budget, work).await {
        Ok(outcome) => {
            tracing::debug!(?outcome, "cancelled work stopped after the client left")
        }
        Err(_) => tracing::warn!(
            budget_secs = budget.as_secs(),
            "cancelled work did not stop within the settle budget"
        ),
    }
}

#[cfg(test)]
mod tests;
