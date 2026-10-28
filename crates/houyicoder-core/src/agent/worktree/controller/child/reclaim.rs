//! Reclaim of a per-child worktree: the record an abandoned child leaves for
//! the shutdown sweep, and the launch-once completion the cleanup path and the
//! sweep share so one tree is never reclaimed twice.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use tokio::sync::Notify;
use tokio::task::spawn_blocking;

use super::ChildCleanup;
use crate::agent::worktree::cleanup::lookup_pending;
use crate::agent::worktree::identify::listed_in_git;
use crate::agent::worktree::session::{has_worktree_changes, remove_clean_worktree};

/// What an aborted child leaves behind for the shutdown sweep: the identity a
/// conservative reclaim needs, and nothing that a live handle already owns.
#[derive(Clone)]
pub(crate) struct WorktreeReclaimRecord {
    pub(crate) repo_root: PathBuf,
    pub(crate) worktree_path: PathBuf,
    pub(crate) worktree_branch: String,
    pub(crate) slug: String,
    /// The commit the tree started from, which a diff counts against. None
    /// when the creation could not read it, which leaves the tree's
    /// cleanliness unprovable and keeps the reclaim from removing it.
    pub(crate) head_commit: Option<String>,
}

/// A launch-once completion cell for one per-child reclaim. The blocking Git
/// reclaim writes its outcome here exactly once; any awaiter (the normal
/// cleanup path or the shutdown sweep) reads it. The cell is shared via Arc,
/// so an aborted awaiter does not take the result with it: the spawn_blocking
/// task is detached on launch (its JoinHandle is dropped), runs to completion
/// regardless of which caller started waiting, and lands the outcome in the
/// cell. A second awaiter (the shutdown sweep arriving after the cleanup
/// awaiter was aborted) sees the same outcome without relaunching Git or
/// re-checking the directory.
///
/// Awaiters are sequential, not concurrent: the shutdown sweep runs only after
/// every child driver has joined, so no live cleanup awaiter is parked on the
/// cell when the sweep arrives. notify_one (which stashes one permit when no
/// waiter is registered) is therefore enough: a set that lands before the
/// awaiter registers is not lost, and there is never a second concurrent
/// awaiter to miss a wake.
pub(crate) struct ReclaimCompletion {
    /// Held only for the instant of reading or writing the Option; never
    /// across an await. The blocking task writes; awaiters read and clone.
    outcome: Mutex<Option<ChildCleanup>>,
    notify: Notify,
}

impl ReclaimCompletion {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            outcome: Mutex::new(None),
            notify: Notify::new(),
        })
    }

    /// The cell, unsynchronized across a poisoning. Nothing runs between the
    /// lock and the release, so the guarded value is whole whether or not the
    /// lock was poisoned, and a panic elsewhere must not cost an awaiter its
    /// outcome.
    fn lock(&self) -> MutexGuard<'_, Option<ChildCleanup>> {
        self.outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Write the outcome and wake one awaiter. Called exactly once, from the
    /// detached blocking reclaim task.
    pub(crate) fn set(&self, outcome: ChildCleanup) {
        *self.lock() = Some(outcome);
        self.notify.notify_one();
    }

    /// Return the outcome once the blocking reclaim has landed it. The
    /// check-then-wait loop tolerates a set that arrived before registration
    /// (the stashed permit wakes the parked awaiter; the next loop iteration
    /// reads the cell) and a set that arrives while parked (the wake fires).
    pub(crate) async fn await_outcome(&self) -> ChildCleanup {
        loop {
            {
                let guard = self.lock();
                if let Some(outcome) = guard.as_ref() {
                    return outcome.clone();
                }
            }
            self.notify.notified().await;
        }
    }
}

/// One entry in the reclaim map: the recovery record plus the launch-once
/// completion cell. launched is the no-duplicate-reclaim guard, mutated only
/// under the map lock. A slot whose child was force-aborted before any cleanup
/// keeps launched false until the shutdown sweep launches it; a slot whose
/// cleanup was aborted mid-reclaim keeps launched true so the sweep awaits the
/// already-running task instead of starting a second one.
pub(crate) struct ReclaimSlot {
    pub(crate) record: WorktreeReclaimRecord,
    pub(crate) completion: Arc<ReclaimCompletion>,
    pub(crate) launched: bool,
}

impl ReclaimSlot {
    pub(crate) fn new(record: WorktreeReclaimRecord) -> Self {
        Self {
            record,
            completion: ReclaimCompletion::new(),
            launched: false,
        }
    }

    /// Start the blocking reclaim if this slot has not started one. The
    /// launched guard (mutated under the map lock) is the single authority: a
    /// second caller arriving while the first task is still running finds
    /// launched true and skips the launch, then awaits the same completion.
    /// The JoinHandle is dropped on launch; the blocking pool runs the task to
    /// completion detached, and the outcome lands in the cell whether or not
    /// any awaiter survives. A panic in the reclaim body reports Kept: the tree
    /// it left behind is not proven clean, so it stays on disk rather than
    /// being treated as removed.
    pub(crate) fn launch(&mut self) {
        if self.launched {
            return;
        }
        self.launched = true;
        let completion = Arc::clone(&self.completion);
        let record = self.record.clone();
        spawn_blocking(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| run_conservative_reclaim(&record)))
                .unwrap_or_else(|payload| reclaim_panic_outcome(&record, payload.as_ref()));
            completion.set(outcome);
        });
    }
}

/// What a reclaim that panicked reports. The tree it left behind is not proven
/// clean, so it stays on disk and the caller learns the tree was kept rather
/// than removed; the log line carries what the panic said, because a sweep that
/// keeps a tree without saying why leaves nothing to act on.
fn reclaim_panic_outcome(
    record: &WorktreeReclaimRecord,
    payload: &(dyn Any + Send),
) -> ChildCleanup {
    tracing::warn!(
        "child worktree reclaim panicked for {}: {}",
        record.slug,
        panic_text(payload)
    );
    ChildCleanup::Kept {
        worktree_path: record.worktree_path.clone(),
        worktree_branch: record.worktree_branch.clone(),
    }
}

/// The text a panic carried, so the log line says what went wrong rather than
/// only that something did.
fn panic_text(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

/// Whether a cleanup record lets the reclaim skip the cleanliness probe.
///
/// The record is written before the Git step, so it only counts once Git has
/// stopped listing the tree: while the tree is still registered, or its listing
/// cannot be read, the probe decides. A record takes the place of a proof the
/// removal already made, not of the proof itself.
pub(crate) fn resumes_leftover(repo_root: &Path, worktree_path: &Path, recorded: bool) -> bool {
    recorded && matches!(listed_in_git(repo_root, worktree_path), Ok(false))
}

/// The conservative reclaim body (sync): prove the tree clean, then remove it.
/// A tree that still holds changes, whose baseline was never recorded, or
/// whose remove fails is kept; only a proven-clean tree whose conservative
/// remove succeeds reports Removed. Runs inside spawn_blocking so sync Git
/// never touches the async worker.
///
/// A leftover recorded by an earlier removal is reclaimed without the
/// cleanliness probe: that removal was already cleared to take the tree apart
/// and it named the target, and the probe cannot read a tree whose Git
/// registration is gone, so asking it would keep the leftover forever. The
/// record only counts once Git has stopped listing the tree, because it is
/// written before the Git step; a tree Git still registers, or one whose
/// listing cannot be read, goes through the probe like any other, which is what
/// keeps a tree holding work from being removed on the strength of a record
/// alone.
pub(crate) fn run_conservative_reclaim(record: &WorktreeReclaimRecord) -> ChildCleanup {
    let pending = match lookup_pending(&record.repo_root, &record.slug) {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(
                "child worktree {} kept: its cleanup record could not be read: {error}",
                record.slug
            );
            return ChildCleanup::Kept {
                worktree_path: record.worktree_path.clone(),
                worktree_branch: record.worktree_branch.clone(),
            };
        }
    };
    if !resumes_leftover(&record.repo_root, &record.worktree_path, pending.is_some()) {
        let clean = record
            .head_commit
            .as_deref()
            .is_some_and(|head| !has_worktree_changes(&record.worktree_path, head));
        if !clean {
            tracing::info!(
                "child worktree {} kept: it holds work or its baseline was never read",
                record.slug
            );
            return ChildCleanup::Kept {
                worktree_path: record.worktree_path.clone(),
                worktree_branch: record.worktree_branch.clone(),
            };
        }
    }
    match remove_clean_worktree(&record.repo_root, &record.slug) {
        Ok(()) => ChildCleanup::Removed {
            worktree_path: record.worktree_path.clone(),
        },
        Err(e) => {
            tracing::warn!(
                "child worktree reclaim failed to remove {}: {e}",
                record.slug
            );
            ChildCleanup::Kept {
                worktree_path: record.worktree_path.clone(),
                worktree_branch: record.worktree_branch.clone(),
            }
        }
    }
}

#[cfg(test)]
mod tests;
