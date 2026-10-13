//! Session write-lease and lifecycle scope.
//!
//! Owns the pairing of the session write lock with its host lifecycle and
//! active child tasks. Releasing a lease joins all host tasks and child
//! drivers before dropping the lock, guaranteeing that writers finish before
//! another process can acquire.

use std::sync::{Arc, Mutex};

use houyicoder_core::agent::Runner;
use houyicoder_service::composition::host_lifecycle::{HostLifecycle, ShutdownReport};
use houyicoder_tui::composition::shared_runtime;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::session_lock::SessionLock;

/// A host lifecycle owner holding the session's exclusive lock. The
/// lock outlives every writer the host owns: releasing the lease joins
/// the host's tasks to completion and drops the lock only after, so a
/// second process cannot acquire while a writer is still flushing. A
/// switch releases the old lease after the new bundle builds, so a
/// failed switch leaves the user on the live session with its lease
/// intact. The sid is the canonical display string the lock was acquired
/// for, so a same-session resume is a no-op before re-acquiring a lock
/// this process already holds.
pub(crate) struct HostLease {
    host: HostLifecycle,
    lock: Option<SessionLock>,
    sid: Option<String>,
    /// The runner whose children the release must join before dropping the
    /// lock. The serve task's abortability makes it unfit to own child
    /// completion; the release task -- the lock owner -- is. None for hosts
    /// with no spawn port attached.
    children: Option<Arc<Runner>>,
}

impl HostLease {
    #[cfg(test)]
    pub(crate) fn new(host: HostLifecycle) -> Self {
        Self {
            host,
            lock: None,
            sid: None,
            children: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        host: HostLifecycle,
        lock: Option<SessionLock>,
        sid: Option<String>,
    ) -> Self {
        Self {
            host,
            lock,
            sid,
            children: None,
        }
    }

    /// A lease already holding a session's exclusive lock. Production entries
    /// build this after admitting (existing sid) or sealing (fresh sid) the
    /// lock, so the lock outlives the lease's writers.
    pub(crate) fn holding(
        host: HostLifecycle,
        lock: SessionLock,
        sid: String,
        children: Option<Arc<Runner>>,
    ) -> Self {
        Self {
            host,
            lock: Some(lock),
            sid: Some(sid),
            children,
        }
    }

    /// An unlocked lease created when lock acquisition fails or before lock
    /// is acquired. Releasing this joins the host tasks and any started
    /// children without holding a lock.
    pub(crate) fn unlocked(host: HostLifecycle, children: Option<Arc<Runner>>) -> Self {
        Self {
            host,
            lock: None,
            sid: None,
            children,
        }
    }

    /// Move the lock into a release task that joins every host task to
    /// completion before dropping the lock. The cancel is signalled before
    /// the task exists: begin_shutdown fires the token synchronously at the
    /// call, so by the time the runtime polls the release task every writer
    /// has already been told to stop. A writer that ignores the token and
    /// exits only on its own gate keeps the lock held until it finishes; a
    /// second process cannot acquire until the join completes and the lock
    /// drops. The task returns the shutdown report so a panicked writer is
    /// reported, not swallowed. The owner must retain or await the returned
    /// handle -- aborting the release task (via the handle) drops the lock
    /// early.
    ///
    /// Child completion runs here too, after the tracked tasks join and
    /// before the lock drops: this task -- the lock owner -- is the one place
    /// the child-join cannot be detached from, where a serve-task abort
    /// cannot orphan it. The runner clone keeps the spawn handle alive for
    /// the join.
    pub(crate) fn begin_release(self, handle: &Handle) -> JoinHandle<ShutdownReport> {
        let shutdown = self.host.begin_shutdown();
        let lock = self.lock;
        let children = self.children;
        handle.spawn(async move {
            let report = shutdown.await;
            if let Some(runner) = children {
                runner.join_children().await;
            }
            drop(lock);
            report
        })
    }
}

/// The owner that tracks the current lease and every release task a session
/// switch started but has not yet confirmed finished. A switch does not drop
/// the old lease's release handle -- it moves the handle into pending, so exit
/// joins each one in turn instead of relying on process teardown to clean up
/// writers that were still flushing when the next session took over. The lock
/// for every lease stays held until its own writers finish; only then does
/// the release drop it.
#[derive(Default)]
pub(crate) struct HostLeaseScope {
    current: Option<HostLease>,
    pending: Vec<JoinHandle<ShutdownReport>>,
}

impl HostLeaseScope {
    pub(crate) fn with_current(lease: HostLease) -> Self {
        Self {
            current: Some(lease),
            pending: Vec::new(),
        }
    }

    /// Read the current lease's session id, if one is installed and locked.
    pub(crate) fn current_sid(&self) -> Option<&str> {
        self.current.as_ref().and_then(|lease| lease.sid.as_deref())
    }

    /// Whether a current lease is installed in the scope.
    #[cfg(test)]
    pub(crate) fn has_current(&self) -> bool {
        self.current.is_some()
    }

    /// Release the current lease into a tracked release task and store the
    /// new lease. The old lease's release handle is retained in pending, not
    /// dropped, so a rapid switch-then-exit cannot abandon a writer that is
    /// still joining.
    pub(crate) fn install(&mut self, lease: HostLease, handle: &Handle) {
        if let Some(old) = self.current.take() {
            self.pending.push(old.begin_release(handle));
        }
        self.current = Some(lease);
    }

    /// Track a release task that has no lease: a build that spawned its host
    /// tasks but failed before acquiring a lock. Exit joins it through
    /// release_all so the failed host's tasks do not outlive the process on
    /// the shared runtime, holding the runner's Arc until they drain.
    pub(crate) fn track_release(&mut self, handle: JoinHandle<ShutdownReport>) {
        self.pending.push(handle);
    }
}

/// The slot a run loop holds the lease scope in. Shared with the resume
/// closure so a switch can install the new lease under the mutex.
pub(crate) type HostLeaseSlot = Arc<Mutex<HostLeaseScope>>;

/// Release the current lease and join every release task the owner holds --
/// the current session's and every prior switch's. The lock for each lease
/// stays held until its own writers finish; only then does the release drop
/// it. Each release task gets the per-task shutdown budget to join its
/// writers, so the total wait is the sum across releases still draining, not
/// a single ceiling -- a switch that stacked several still-draining releases
/// joins them in turn. Called on the main thread, not a runtime worker --
/// Handle::block_on panics from inside a worker.
pub(crate) fn release_all(host_slot: &HostLeaseSlot) {
    let runtime = shared_runtime();
    let handle = runtime.handle();
    let (current, mut pending) = {
        // Poison-tolerant on purpose: an unwind that panicked in a slot
        // critical section must still join the writers and drop the locks.
        // A panic here would mask the original unwind.
        let mut scope = host_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = scope.current.take();
        let pending = std::mem::take(&mut scope.pending);
        (current, pending)
    };
    if let Some(lease) = current {
        pending.push(lease.begin_release(handle));
    }
    for release in pending {
        match handle.block_on(release) {
            Ok(report) => report.trace_failures("host lease release"),
            Err(e) => tracing::warn!("host lease release task did not finish: {e}"),
        }
    }
}

/// Unwind backstop for a run loop's lease scope. The drop is the single
/// release door -- the normal return, the error return, and a panic
/// unwinding past the loop all go through it -- so a crash cannot drop the
/// lock while its writers still run. The shared runtime outlives any
/// unwind, so the block_on inside the drop cannot fail.
pub(crate) struct LeaseScopeGuard {
    slot: HostLeaseSlot,
}

impl LeaseScopeGuard {
    pub(crate) fn new(slot: &HostLeaseSlot) -> Self {
        Self {
            slot: Arc::clone(slot),
        }
    }
}

impl Drop for LeaseScopeGuard {
    fn drop(&mut self) {
        release_all(&self.slot);
    }
}

#[cfg(test)]
mod tests;
