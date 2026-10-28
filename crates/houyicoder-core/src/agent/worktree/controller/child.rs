//! Per-child worktrees: the tree a delegated child works in, its fence guard,
//! and the reclaim that removes the tree once, either when the child finishes
//! or at the shutdown sweep for a child that never got that far. A child's tree
//! is separate from the session's own worktree: the parent keeps its working
//! directory and its wide fence.

use std::collections::HashMap;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::MutexGuard;
use std::sync::{Arc, Mutex};

use houyicoder_api::sandbox::WorktreeFenceGuard;
use houyicoder_context::HookEventKind;

use self::reclaim::ReclaimCompletion;
use self::reclaim::ReclaimSlot;
use self::reclaim::WorktreeReclaimRecord;
use super::WorktreeController;
use crate::agent::worktree::record::WorktreeLease;
use crate::agent::worktree::session as worktree_session;
use crate::agent::worktree::session::WorktreeError;

/// A per-child worktree: the path, the branch, and the fence guard the child
/// holds during its run. The guard keeps the child's sandbox execs fenced to
/// the worktree; cleanup_child restores the wide fence, then removes the
/// worktree when the child left no changes or preserves it for the caller to
/// continue on the branch when it did. Not Clone -- the guard is single-owner.
///
/// The guard's own Drop restores the wide fence when the handle is dropped
/// without a cleanup_child call (a force-aborted child). The controller keeps a
/// separate reclaim record per child so that abandoned tree is removed at
/// shutdown instead of leaking.
pub struct ChildWorktree {
    pub worktree_path: PathBuf,
    pub worktree_branch: String,
    slug: String,
    pub fence_guard: WorktreeFenceGuard,
    /// Held for the child's run and released before its tree is reclaimed, so
    /// the reclaim is not refused by the child's own hold and no other session
    /// enters the tree meanwhile.
    lease: WorktreeLease,
}

/// The outcome of cleaning up a per-child worktree at terminal state. A clean
/// worktree (no uncommitted files, no new commits) is auto-removed; a dirty one
/// is preserved so the caller can continue on the branch, with the path and
/// branch reported back.
#[derive(Debug, Clone)]
pub enum ChildCleanup {
    Removed {
        worktree_path: PathBuf,
    },
    Kept {
        worktree_path: PathBuf,
        worktree_branch: String,
    },
}

/// The per-child trees this controller created and has not yet reclaimed, by
/// slug. A tree is recorded when it is created, so a child aborted before its
/// cleanup still leaves a record the shutdown sweep can act on. Each record
/// carries a launch-once completion that the cleanup path and the sweep share,
/// so one tree is never reclaimed twice.
#[derive(Default)]
pub(super) struct ChildReclaimLedger {
    slots: Mutex<HashMap<String, ReclaimSlot>>,
}

impl ChildReclaimLedger {
    /// Record a tree as in use by a child that is about to run. A tree that
    /// already existed is not recorded by the caller: it carries a prior
    /// child's output, which a reclaim would read as no new changes and delete.
    fn record(&self, record: WorktreeReclaimRecord) {
        let slug = record.slug.clone();
        self.slots
            .lock()
            .expect("reclaims lock")
            .insert(slug, ReclaimSlot::new(record));
    }

    /// Start the reclaim for one slug if it has not started, and hand back the
    /// completion both the cleanup path and the sweep wait on. None when no
    /// record is held, which a sweep that already finished the tree leaves
    /// behind.
    fn claim(&self, slug: &str) -> Option<Arc<ReclaimCompletion>> {
        let mut slots = self.slots.lock().expect("reclaims lock");
        let slot = slots.get_mut(slug)?;
        slot.launch();
        Some(Arc::clone(&slot.completion))
    }

    /// Start every recorded reclaim. A slot whose cleanup already started one
    /// is claimed rather than launched again, so the sweep awaits the running
    /// task instead of racing it with a second directory re-check.
    fn claim_all(&self) -> Vec<Arc<ReclaimCompletion>> {
        self.slots
            .lock()
            .expect("reclaims lock")
            .values_mut()
            .map(|slot| {
                slot.launch();
                Arc::clone(&slot.completion)
            })
            .collect()
    }

    /// Forget a slug whose reclaim outcome was observed.
    fn forget(&self, slug: &str) {
        self.slots.lock().expect("reclaims lock").remove(slug);
    }

    /// Forget every slug once the sweep has observed each outcome.
    fn clear(&self) {
        self.slots.lock().expect("reclaims lock").clear();
    }

    /// The held slots, for a test that plants the reclaim task an aborted
    /// cleanup would have left running.
    #[cfg(test)]
    pub(super) fn held_for_test(&self) -> MutexGuard<'_, HashMap<String, ReclaimSlot>> {
        self.slots.lock().expect("reclaims lock")
    }
}

impl WorktreeController {
    /// Create a per-child worktree and restrict the fence to it, leaving the
    /// parent on its original working directory and wide fence. The returned
    /// guard travels with the child handle and is held for the child's run;
    /// Drop restores the wide fence on completion. Refused while a sandbox exec
    /// is running (that exec keeps its wide-fence profile and could escape the
    /// restriction). Fail-closed: a fence failure returns an error, never a
    /// degraded no-isolation run.
    pub async fn enter_for_child(&self, slug: String) -> Result<ChildWorktree, WorktreeError> {
        if self.sandbox.active_exec_count() > 0 {
            return Err(WorktreeError::Git {
                stderr: "per-child worktree refused: a sandbox exec is in flight".into(),
            });
        }
        // The child's tree is marked in use for the child's run; cleanup
        // releases the lease before it asks for the reclaim. The lease comes
        // first so a refusal leaves no tree behind for nobody to reclaim.
        let lease = WorktreeLease::acquire(&self.repo_root, &slug)?;
        let created = match worktree_session::get_or_create_worktree(&self.repo_root, &slug) {
            Ok(created) => created,
            Err(error) => {
                // The lease was taken for a tree that was never made, so the
                // file it created is residue nothing else would clear.
                lease.discard();
                return Err(error);
            }
        };
        // Record before the fence is restricted and before the hook-fire
        // await, so a failure or abort in either still leaves the tree in the
        // reclaim ledger for the shutdown sweep. Only a freshly created tree is
        // recorded: a resumed one is the caller's to keep, so it never enters
        // the sweep.
        if !created.existed {
            self.reclaims.record(WorktreeReclaimRecord {
                repo_root: self.repo_root.clone(),
                worktree_path: created.worktree_path.clone(),
                worktree_branch: created.worktree_branch.clone(),
                slug: slug.clone(),
                head_commit: created.head_commit.clone(),
            });
        }
        let guard = self
            .sandbox
            .narrow_to_worktree(&created.worktree_path, &self.git_common_dir)
            .map_err(|e| WorktreeError::Git {
                stderr: format!("narrow_to_worktree: {e}"),
            })?;
        self.fire_worktree_event(HookEventKind::WorktreeCreate, created.worktree_path.clone())
            .await;
        Ok(ChildWorktree {
            worktree_path: created.worktree_path,
            worktree_branch: created.worktree_branch,
            slug,
            fence_guard: guard,
            lease,
        })
    }

    /// Clean up a per-child worktree at terminal state: restore the wide
    /// fence, then remove the tree when the child left no changes or preserve
    /// it (with path and branch) when it did. Infallible: an abandoned tree is
    /// a cleanup outcome the caller observes, not an error to propagate at
    /// finalize. The fence restore happens here (the child handle carries the
    /// guard); the controller then runs the same conservative reclaim as the
    /// shutdown sweep, removing its reclaim record only after that reclaim
    /// finishes (an abort mid-reclaim leaves the record for the sweep). A
    /// normal cleanup and an abandoned-child reclaim share one removal path.
    pub async fn cleanup_child(&self, cw: ChildWorktree) -> ChildCleanup {
        let ChildWorktree {
            worktree_path,
            worktree_branch,
            slug,
            fence_guard,
            lease,
        } = cw;
        if let Err(e) = fence_guard.restore() {
            tracing::warn!("child worktree fence restore failed: {e}");
        }
        // The child is done with the tree, so its hold ends here: the reclaim
        // below refuses a leased tree, and this session's own lease is not a
        // reason to keep one.
        drop(lease);
        // Claim the reclaim if it has not started already, then await the
        // shared completion. The slot stays in the ledger through the await so
        // an abort mid-reclaim leaves the launched task's completion reachable
        // for the shutdown sweep; the slot is removed only once the outcome is
        // observed.
        let outcome = match self.reclaims.claim(&slug) {
            Some(completion) => completion.await_outcome().await,
            // No reclaim record: the entry was already swept by a shutdown
            // sweep. Report the tree kept so the caller does not delete it a
            // second time.
            None => {
                return ChildCleanup::Kept {
                    worktree_path,
                    worktree_branch,
                };
            }
        };
        self.reclaims.forget(&slug);
        // The normal path fires WorktreeRemove only when the removal actually
        // succeeded; the shutdown sweep removes without firing (see
        // reclaim_abandoned) so an aborted child's silent recovery never
        // over-reports a lifecycle event.
        if let ChildCleanup::Removed { worktree_path } = &outcome {
            self.fire_worktree_event(HookEventKind::WorktreeRemove, worktree_path.clone())
                .await;
        }
        outcome
    }

    /// Reclaim every per-child worktree still recorded (a child force-aborted
    /// before its finalize cleanup tail, whose slot cleanup_child never
    /// removed). Called by the shutdown owner after all child drivers have
    /// joined and before the parent lease is released, so the blocking reclaim
    /// is observed to completion rather than left to a Drop that would run
    /// sync Git on the worker.
    ///
    /// A slot whose cleanup was aborted mid-reclaim is already launched; the
    /// sweep awaits that launched task's completion rather than relaunching Git,
    /// so a blocked branch deletion (a Git ref lock held by another process) is
    /// waited out, not raced by a second directory re-check. A slot whose child
    /// never reached cleanup is launched here for the first time. The sweep
    /// removes a clean tree without firing WorktreeRemove: the abort already
    /// left the child's lifecycle event unbalanced, and the silent recovery does
    /// not manufacture a removal event for it.
    pub async fn reclaim_abandoned(&self) {
        // The slots stay in the ledger through the await (a late cleanup_child
        // for the same slug would await the same completion rather than find
        // the slot drained); the ledger is cleared only once every outcome is
        // observed.
        for completion in self.reclaims.claim_all() {
            completion.await_outcome().await;
        }
        self.reclaims.clear();
    }
}

pub(super) mod reclaim;
