//! Owner for a local host's session-scoped background tasks.

use std::future::Future;
use std::time::Duration;

use houyicoder_async::CancellationToken;
use tokio::task::{JoinError, JoinHandle};
use tokio::time::timeout;

/// The production budget a stuck task gets before the reaper aborts it.
#[cfg(not(test))]
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Under test the budget shrinks so the abort path runs in milliseconds,
/// not the full production wait.
#[cfg(test)]
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(20);

/// How one tracked task resolved during shutdown. The owner observes this
/// instead of a void result so a panicked writer is reported to tracing,
/// not swallowed.
pub enum TaskOutcome {
    /// The task returned before the shutdown budget elapsed.
    Joined,
    /// The task ignored the cancel token and was aborted after the budget,
    /// then awaited so the abort is confirmed rather than left pending.
    Aborted,
    /// The task panicked; the JoinError carries the panic payload.
    Panicked(JoinError),
}

/// A per-task record of how shutdown resolved, returned by begin_shutdown
/// so the caller observes every outcome. The report's existence is the
/// ending confirmation: begin_shutdown joins every tracked task — by
/// return, abort, or its own panic — before the future completes. How each
/// ending went is a separate fact, answered by panic_count.
pub struct ShutdownReport {
    outcomes: Vec<TaskOutcome>,
}

impl ShutdownReport {
    /// How many tasks ended by panicking. They have stopped, but not
    /// orderly — state they were writing may be inconsistent. Zero is a
    /// clean stop.
    pub fn panic_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| matches!(outcome, TaskOutcome::Panicked(_)))
            .count()
    }

    /// Trace every non-clean outcome at warn level so a writer panic or a
    /// forced abort is not lost when the release task itself completed. A
    /// task that ignored the cancel token and had to be force-killed is a
    /// degraded condition the operator must learn about, not a clean stop.
    pub fn trace_failures(&self, label: &str) {
        for (i, outcome) in self.outcomes.iter().enumerate() {
            match outcome {
                TaskOutcome::Joined => {}
                TaskOutcome::Aborted => {
                    tracing::warn!("{label} task {i} ignored cancel and was force-aborted");
                }
                TaskOutcome::Panicked(err) => {
                    tracing::warn!("{label} task {i} did not finish: {err}");
                }
            }
        }
    }

    /// Read-only access for tests asserting a specific task's outcome.
    pub fn outcomes(&self) -> &[TaskOutcome] {
        &self.outcomes
    }
}

/// Owns the cancel token and join handles for the serve, drain, and relay
/// tasks a self-built local host spawns. Built at the composition root and
/// handed to the TUI loop; the resume path reaps an old owner before
/// building a new one so a session switch does not leak the prior tasks or
/// the Runner. The drain and relay get the token; the serve exits on its
/// own when its client drops, and the owner still reaps its handle.
#[derive(Default)]
pub struct HostLifecycle {
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl HostLifecycle {
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            tasks: Vec::new(),
        }
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn track(&mut self, handle: JoinHandle<()>) {
        self.tasks.push(handle);
    }

    /// Cancel the token and return a future that joins every tracked task
    /// with a bounded wait, producing a per-task report the caller observes.
    /// The cancel fires synchronously at the call, before the future is
    /// constructed, so a release owner that obtains this future before
    /// starting its release task has already signalled every writer to stop
    /// by the time the task exists. A task that does not exit within the
    /// budget is aborted and then awaited so the abort is confirmed, not
    /// fire-and-forget. The caller drives the future — no detached reaper —
    /// so the lock owner can hold the guard until every writer has actually
    /// stopped.
    pub fn begin_shutdown(self) -> impl Future<Output = ShutdownReport> + Send {
        self.cancel.cancel();
        let tasks = self.tasks;
        async move {
            let mut outcomes = Vec::with_capacity(tasks.len());
            for mut task in tasks {
                match timeout(SHUTDOWN_TIMEOUT, &mut task).await {
                    Ok(Ok(())) => outcomes.push(TaskOutcome::Joined),
                    Ok(Err(err)) if err.is_panic() => outcomes.push(TaskOutcome::Panicked(err)),
                    // A task cancelled before the budget (an external abort we
                    // did not issue) still counts as stopped, not panicked.
                    Ok(Err(_)) => outcomes.push(TaskOutcome::Aborted),
                    Err(_) => {
                        task.abort();
                        outcomes.push(match task.await {
                            Err(err) if err.is_panic() => TaskOutcome::Panicked(err),
                            _ => TaskOutcome::Aborted,
                        });
                    }
                }
            }
            ShutdownReport { outcomes }
        }
    }
}

#[cfg(test)]
mod tests;
