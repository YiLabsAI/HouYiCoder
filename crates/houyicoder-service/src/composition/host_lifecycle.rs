//! Owner for a local host's session-scoped background tasks.

use std::sync::Arc;
use std::time::Duration;

use houyicoder_async::CancellationToken;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

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
                if tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut task)
                    .await
                    .is_err()
                {
                    task.abort();
                }
            }
        });
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
}
