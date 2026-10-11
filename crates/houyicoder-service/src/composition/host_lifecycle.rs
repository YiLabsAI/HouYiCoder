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
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::oneshot::channel;

    /// A task that exits when its token cancels, used to stand in for the
    /// drain and relay without pulling their engine collaborators in.
    fn token_task(token: CancellationToken, done: Arc<AtomicBool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            tokio::select! {
                _ = token.cancelled() => done.store(true, Ordering::SeqCst),
                _ = std::future::pending::<()>() => {}
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
            std::future::pending::<()>().await;
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
}
