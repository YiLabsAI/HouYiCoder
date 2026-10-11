//! Owner for a local host's session-scoped background tasks.

use std::sync::Arc;
use std::time::Duration;

use houyicoder_async::CancellationToken;
use tokio::time::timeout;

/// The production budget a stuck task gets before the reaper aborts it.
#[cfg(not(test))]
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Under test the budget shrinks so the abort path runs in milliseconds,
/// not the full production wait.
#[cfg(test)]
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(20);

/// Owns the cancel token and join handles for the serve, drain, and relay
/// tasks a self-built local host spawns. Built at the composition root and
/// handed to the TUI loop; the resume path reaps an old owner before
/// building a new one so a session switch does not leak the prior tasks or
/// the Runner. The drain and relay get the token; the serve exits on its
/// own when its paired client drops, and the owner still reaps its handle.
#[derive(Default)]
pub struct HostLifecycle {
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
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

    pub fn track(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.tasks.push(handle);
    }

    /// Cancel the drain and relay, then reap each task with a bounded wait;
    /// a task that does not exit in time is aborted. The reaper runs detached
    /// on the given handle so a synchronous caller returns at once.
    pub fn shutdown(self, handle: &tokio::runtime::Handle) {
        self.cancel.cancel();
        let tasks = self.tasks;
        handle.spawn(async move {
            for mut task in tasks {
                if timeout(SHUTDOWN_TIMEOUT, &mut task).await.is_err() {
                    task.abort();
                }
            }
        });
    }

    /// Cancel the token and return a future that joins every tracked task
    /// with a bounded wait. The caller observes completion: a task that
    /// times out is aborted and then awaited so the abort is confirmed,
    /// not fire-and-forget. No detached reaper is started — the caller
    /// drives the future, so the lock owner can hold the guard until
    /// every writer has actually stopped.
    pub fn begin_shutdown(self) -> impl std::future::Future<Output = ()> + Send {
        self.cancel.cancel();
        let tasks = self.tasks;
        async move {
            for mut task in tasks {
                if timeout(SHUTDOWN_TIMEOUT, &mut task).await.is_err() {
                    task.abort();
                    drop(task.await);
                }
            }
        }
    }
}

/// A clonable slot a resume closure uses to swap owners: take the old one to
/// reap it, store the new one.
pub type HostSlot = Arc<std::sync::Mutex<Option<HostLifecycle>>>;

pub fn empty_slot() -> HostSlot {
    Arc::new(std::sync::Mutex::new(None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// A task that exits when its token cancels, used to stand in for the
    /// drain and relay without pulling their engine collaborators in.
    fn token_task(token: CancellationToken, done: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tokio::select! {
                _ = token.cancelled() => done.store(true, Ordering::SeqCst),
                _ = std::future::pending::<()>() => {}
            }
        })
    }

    /// shutdown cancels the token the tracked task selects on, so the task
    /// exits and the detached reaper observes its completion. This is the
    /// cancel-then-wait contract; the abort fallback covers a task that
    /// ignores its token.
    #[tokio::test]
    async fn test_shutdown_cancels_token() {
        let mut host = HostLifecycle::new();
        let done = Arc::new(AtomicBool::new(false));
        host.track(token_task(host.cancel_token(), Arc::clone(&done)));
        host.shutdown(&tokio::runtime::Handle::current());
        for _ in 0..50 {
            if done.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(done.load(Ordering::SeqCst), "shutdown cancels the token");
    }

    /// Repeated shutdown + install cycles (the in-process resume shape) each
    /// reap their own tasks. A leak here would leave old tasks looping across
    /// switches; the test asserts each switch's task exits on its own token.
    #[tokio::test]
    async fn test_shutdown_reaps_across_switches() {
        for _ in 0..3 {
            let mut host = HostLifecycle::new();
            let done = Arc::new(AtomicBool::new(false));
            host.track(token_task(host.cancel_token(), Arc::clone(&done)));
            host.shutdown(&tokio::runtime::Handle::current());
            for _ in 0..50 {
                if done.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(done.load(Ordering::SeqCst), "each switch reaps its task");
        }
    }

    /// begin_shutdown joins a task that exits on its own gate, so the future
    /// completes only after the task actually finishes — not when the cancel
    /// is called. A caller that treats the cancel as completion would return
    /// before done is set; awaiting begin_shutdown must see done true.
    #[tokio::test]
    async fn test_begin_shutdown_joins() {
        let mut host = HostLifecycle::new();
        let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
        let done = Arc::new(AtomicBool::new(false));
        let done_c = Arc::clone(&done);
        host.track(tokio::spawn(async move {
            let _ = gate_rx.await;
            done_c.store(true, Ordering::SeqCst);
        }));
        let shutdown = host.begin_shutdown();
        drop(gate_tx);
        shutdown.await;
        assert!(
            done.load(Ordering::SeqCst),
            "begin_shutdown awaited the task"
        );
    }

    /// A task that ignores the cancel token and blocks forever cannot exit on
    /// its own. begin_shutdown must abort it after the bounded wait and then
    /// await the abort so the caller observes actual completion — the cancel
    /// call or a detached Drop is not join evidence. A guard dropped when the
    /// task's future is cancelled records that the abort actually happened.
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

        host.begin_shutdown().await;

        assert!(
            aborted.load(Ordering::SeqCst),
            "stuck task was aborted, not abandoned"
        );
    }
}
