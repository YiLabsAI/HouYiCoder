//! The worktree session controller: owns the enter/exit lifecycle for the
//! worktree isolation feature. A controller is built at the composition root
//! with the repo root + git common dir + the shared sandbox session + the
//! session log; the caller attaches a Weak handle to the runner after it is
//! Arc-wrapped (staged delegation — the runner is constructed owned, then
//! shared). EnterWorktree and ExitWorktree tools hold an Arc to the
//! controller and delegate to enter/exit.
//!
//! The controller is the single owner of the live worktree session state
//! (an Arc-shared struct so both tools see the same session). enter refuses a
//! second enter (scope guard) and refuses while a sandbox exec is in flight
//! (a backgrounded command would keep its wide-fence profile alive and could
//! still write the main tree after the narrow). exit is fail-closed on
//! discard: a remove with uncommitted changes + discard_changes=false refuses
//! and lists the work so the user confirms before re-invoking with true.
//!
//! Both enter and exit append a SessionLogEntry (WorktreeEnter / WorktreeExit) to
//! the session log so a replay can restore the execution environment later —
//! the record lands now (resume consumption is a separate, deferred task);
//! never silent on the cwd + fence switch.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::Notify;
use tokio::task::spawn_blocking;

use houyicoder_api::agent_event::{EventHandler, UserNoticeEvent};
use houyicoder_api::hook_fire::HookFire;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::session::SessionLog;
use houyicoder_context::{
    EventId, HookEventKind, HookFirePayload, SessionEvent, SessionId, SessionLogEntry,
};

use super::worktree_session::{self, WorktreeError, WorktreeSession};
use houyicoder_api::sandbox::WorktreeFenceGuard;
use std::process::id;

/// Monotonic counter for random worktree slugs (no Math.random — a random
/// name is a nicety; here a counter + pid derives a unique slug
/// deterministically).
static SLUG_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The exit action: keep preserves the worktree + branch on disk; remove
/// deletes both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitAction {
    Keep,
    Remove,
}

/// The result of an enter (rendered into the tool result message).
#[derive(Clone, Debug)]
pub struct EnterResult {
    pub worktree_path: String,
    pub worktree_branch: String,
    pub message: String,
}

/// The result of an exit (rendered into the tool result message).
#[derive(Clone, Debug)]
pub struct ExitOutcome {
    pub action: ExitAction,
    pub original_cwd: String,
    pub worktree_path: String,
    pub worktree_branch: Option<String>,
    pub message: String,
}

/// A per-child worktree: the path + branch + the fence guard the child
/// holds during its run. The guard keeps the child's sandbox execs fenced
/// to the worktree; cleanup_child restores the wide fence, then removes
/// the worktree when the child left no changes or preserves it for the
/// caller to continue on the branch when it did. Not Clone -- the guard
/// is single-owner.
///
/// The guard's own Drop restores the wide fence when the handle is dropped
/// without a cleanup_child call (a force-aborted child). The controller keeps
/// a separate reclaim record per child so that abandoned tree is removed at
/// shutdown instead of leaking.
pub struct ChildWorktree {
    pub worktree_path: PathBuf,
    pub worktree_branch: String,
    slug: String,
    pub fence_guard: WorktreeFenceGuard,
}

/// The outcome of cleaning up a per-child worktree at terminal state. A
/// clean worktree (no uncommitted files, no new commits) is auto-removed;
/// a dirty one is preserved so the caller can continue on the branch, with
/// the path + branch reported back.
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

/// The record a per-child worktree leaves with the controller so an
/// abandoned child still gets its tree reclaimed at shutdown. enter_for_child
/// inserts one before its first await; cleanup_child removes the matching
/// entry only after the reclaim finishes, so an abort mid-reclaim leaves it
/// for reclaim_abandoned to observe. The fence guard itself travels on the
/// child handle, not here.
#[derive(Clone)]
struct WorktreeReclaimRecord {
    repo_root: PathBuf,
    worktree_path: PathBuf,
    worktree_branch: String,
    slug: String,
    head_commit: String,
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
/// Awaiters are sequential, not concurrent: reclaim_abandoned runs only after
/// every child driver has joined, so no live cleanup awaiter is parked on the
/// cell when the sweep arrives. notify_one (which stashes one permit when no
/// waiter is registered) is therefore enough: a set that lands before the
/// awaiter registers is not lost, and there is never a second concurrent
/// awaiter to miss a wake.
struct ReclaimCompletion {
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

    /// Write the outcome and wake one awaiter. Called exactly once, from the
    /// detached blocking reclaim task.
    fn set(&self, outcome: ChildCleanup) {
        if let Ok(mut guard) = self.outcome.lock() {
            *guard = Some(outcome);
        }
        self.notify.notify_one();
    }

    /// Return the outcome once the blocking reclaim has landed it. The
    /// check-then-wait loop tolerates a set that arrived before registration
    /// (the stashed permit wakes the parked awaiter; the next loop iteration
    /// reads the cell) and a set that arrives while parked (the wake fires).
    async fn await_outcome(&self) -> ChildCleanup {
        loop {
            {
                let guard = self.outcome.lock().expect("completion lock");
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
struct ReclaimSlot {
    record: WorktreeReclaimRecord,
    completion: Arc<ReclaimCompletion>,
    launched: bool,
}

/// The conservative reclaim body (sync): prove the tree clean, then remove it.
/// A tree that still holds changes, or whose remove fails, is kept; only a
/// proven-clean tree whose conservative remove succeeds reports Removed. Runs
/// inside spawn_blocking so sync Git never touches the async worker. A panic
/// here is caught by the caller's catch_unwind and degrades to a fail-closed
/// Kept so the awaiter never hangs.
fn run_conservative_reclaim(recovery: &WorktreeReclaimRecord) -> ChildCleanup {
    let dirty =
        worktree_session::has_worktree_changes(&recovery.worktree_path, &recovery.head_commit);
    if dirty {
        return ChildCleanup::Kept {
            worktree_path: recovery.worktree_path.clone(),
            worktree_branch: recovery.worktree_branch.clone(),
        };
    }
    match worktree_session::remove_clean_worktree(&recovery.repo_root, &recovery.slug) {
        Ok(()) => ChildCleanup::Removed {
            worktree_path: recovery.worktree_path.clone(),
        },
        Err(e) => {
            tracing::warn!(
                "child worktree reclaim failed to remove {}: {e}",
                recovery.slug
            );
            ChildCleanup::Kept {
                worktree_path: recovery.worktree_path.clone(),
                worktree_branch: recovery.worktree_branch.clone(),
            }
        }
    }
}

/// Launch the blocking reclaim for one slot, if it has not been launched
/// already. The launched guard (mutated under the map lock) is the single
/// authority: a second caller arriving while the first task is still running
/// finds launched true and skips the launch, then awaits the same completion.
/// The JoinHandle is dropped on launch; the blocking pool runs the task to
/// completion detached, and the outcome lands in the cell whether or not any
/// awaiter survives. A panic in the reclaim body is caught and degrades to a
/// fail-closed Kept so the awaiter never hangs.
fn launch_reclaim(slot: &mut ReclaimSlot) {
    if slot.launched {
        return;
    }
    slot.launched = true;
    let completion = Arc::clone(&slot.completion);
    let record = slot.record.clone();
    spawn_blocking(move || {
        let outcome = catch_unwind(AssertUnwindSafe(|| run_conservative_reclaim(&record)))
            .unwrap_or_else(|_| ChildCleanup::Kept {
                worktree_path: record.worktree_path.clone(),
                worktree_branch: record.worktree_branch.clone(),
            });
        completion.set(outcome);
    });
}

pub struct WorktreeController {
    repo_root: PathBuf,
    git_common_dir: PathBuf,
    session: Mutex<Option<WorktreeSession>>,
    /// A shared handle to the runner's interior-mutable cwd, so enter/exit
    /// can repoint the system-prompt project-context walk-up without a typed
    /// Runner handle. Swappable: the controller is built before the runner
    /// (the tools register into the registry first), then the composition root
    /// calls set_cwd_handle with the runner's real handle once it is built.
    /// Reads/writes through the inner Arc are the same writes the next
    /// ContextBuilder build observes (single source of truth).
    cwd_handle: Mutex<Arc<RwLock<PathBuf>>>,
    sandbox: Arc<dyn SandboxSession>,
    /// The narrow-to-worktree guard; restore on exit (keep + remove both
    /// restore the fence to the repo root before any other action).
    fence_guard: Mutex<Option<WorktreeFenceGuard>>,
    store: Arc<dyn SessionLog>,
    session_id: SessionId,
    /// The main branch ref at enter, so exit can warn (not block) if the
    /// sandboxed agent rewrote it. The fence allows .git refs, so a malicious
    /// or buggy agent could move main; the alert keeps it from being silent.
    original_main_ref: Mutex<Option<String>>,
    /// Optional destination for user-visible notices such as a moved main
    /// branch. The composition root attaches it after constructing the runner.
    user_notice: Mutex<Option<Arc<dyn EventHandler<UserNoticeEvent>>>>,
    /// The hook-fire seam for WorktreeCreate / WorktreeRemove events.
    /// Attached after construction:
    /// the controller is built before the runner, so the composition root
    /// attaches the runner's hook-fire handle once it exists. None when no
    /// hooks are wired; fire is a no-op then.
    hook_fire: Mutex<Option<Arc<dyn HookFire>>>,
    /// Per-child worktrees not yet reclaimed, by slug. Populated at
    /// enter_for_child, removed by cleanup_child once its reclaim is observed,
    /// and swept by reclaim_abandoned at shutdown. Each slot holds the
    /// recovery record plus a launch-once completion cell, so an aborted
    /// cleanup leaves both the record and the launched task's completion
    /// reachable: Drop restores the fence, this map keeps the blocking reclaim
    /// observable to completion off the worker.
    pending_reclaims: Mutex<HashMap<String, ReclaimSlot>>,
}

impl WorktreeController {
    /// Build the controller shell. The cwd handle starts as a dummy (writes
    /// before set_cwd_handle are lost); call set_cwd_handle once the runner is
    /// built so the controller shares the runner's real cwd.
    pub fn new(
        repo_root: PathBuf,
        git_common_dir: PathBuf,
        sandbox: Arc<dyn SandboxSession>,
        store: Arc<dyn SessionLog>,
        session_id: SessionId,
    ) -> Self {
        Self {
            cwd_handle: Mutex::new(Arc::new(RwLock::new(repo_root.clone()))),
            repo_root,
            git_common_dir,
            session: Mutex::new(None),
            sandbox,
            fence_guard: Mutex::new(None),
            store,
            session_id,
            original_main_ref: Mutex::new(None),
            user_notice: Mutex::new(None),
            hook_fire: Mutex::new(None),
            pending_reclaims: Mutex::new(HashMap::new()),
        }
    }

    /// Swap in the runner's real cwd handle. Call after the runner is built
    /// so enter/exit writes reach the runner's ContextBuilder (the single
    /// source of truth the next system-prompt build reads).
    pub fn set_cwd_handle(&self, cwd: Arc<RwLock<PathBuf>>) {
        *self.cwd_handle.lock().expect("cwd handle lock") = cwd;
    }

    /// Attach the runner's user-notice destination.
    pub fn set_user_notice_handler(&self, handler: Arc<dyn EventHandler<UserNoticeEvent>>) {
        *self.user_notice.lock().expect("user notice lock") = Some(handler);
    }

    /// Attach the hook-fire seam for WorktreeCreate / WorktreeRemove events.
    /// The controller is built before
    /// the runner, so the composition root attaches the runner's hook-fire
    /// handle once it exists. Pass None to clear.
    pub fn set_hook_fire(&self, fire: Option<Arc<dyn HookFire>>) {
        *self.hook_fire.lock().expect("hook_fire lock") = fire;
    }

    /// Fire a WorktreeCreate or WorktreeRemove hook with the worktree path,
    /// when a hook-fire seam is attached. No-op otherwise. The signal lands
    /// in this controller's session log (the parent session at a subagent
    /// spawn, the running session at a tool-driven enter/exit). The Arc is
    /// cloned out of the lock before the await so a std Mutex guard is not
    /// held across the fire future (which writes to the session store).
    async fn fire_worktree_event(&self, event: HookEventKind, path: PathBuf) {
        let hf = self.hook_fire.lock().expect("hook_fire lock").clone();
        if let Some(hf) = hf {
            let payload = HookFirePayload::worktree(self.session_id, path);
            hf.fire(event, payload).await;
        }
    }

    /// True when a worktree session is active (an EnterWorktree ran + no
    /// matching exit yet). Used by the /worktree display + tests.
    pub fn current(&self) -> bool {
        self.session.lock().expect("session lock").is_some()
    }

    /// Enter a worktree session (EnterWorktree tool body). Creates or resumes
    /// the worktree, narrows the fence + cwd, records the main ref + a
    /// WorktreeEnter event. Refuses a second enter + refuses while an exec is
    /// in flight.
    pub async fn enter(&self, slug: Option<String>) -> Result<EnterResult, WorktreeError> {
        {
            let s = self.session.lock().expect("session lock");
            if s.is_some() {
                return Err(WorktreeError::Git {
                    stderr: "already in a worktree session".into(),
                });
            }
        }
        // H10: refuse while a sandbox exec is in flight — a backgrounded
        // command keeps its wide-fence profile alive and could write the main
        // tree after the narrow.
        if self.sandbox.active_exec_count() > 0 {
            return Err(WorktreeError::Git {
                stderr: "worktree enter refused: a sandbox exec is in flight; wait or cancel first"
                    .into(),
            });
        }
        let slug = slug.unwrap_or_else(|| {
            let n = SLUG_COUNTER.fetch_add(1, Ordering::Relaxed);
            format!("wt-{}-{}", id(), n)
        });
        let created = worktree_session::get_or_create_worktree(&self.repo_root, &slug)?;
        let original_branch = worktree_session::current_branch(&self.repo_root);
        // H5: record the main branch ref so exit can warn if it moved.
        let main_ref = original_branch
            .as_deref()
            .and_then(|b| worktree_session::rev_parse(&self.repo_root, &format!("refs/heads/{b}")));
        *self.original_main_ref.lock().expect("main ref lock") = main_ref.clone();
        // Narrow the fence to the worktree + .git allow-back. The guard
        // restores on drop.
        let guard = self
            .sandbox
            .narrow_to_worktree(&created.worktree_path, &self.git_common_dir)
            .map_err(|e| WorktreeError::Git {
                stderr: format!("narrow_to_worktree: {e}"),
            })?;
        *self.fence_guard.lock().expect("fence guard lock") = Some(guard);
        // Switch the cwd (system prompt project-context walk-up). The sandbox
        // exec cwd already followed the narrow; this repoints the
        // ContextBuilder cwd so the next build's AGENTS.md walk-up + env_info
        // reflect the worktree.
        let cwd_handle = self.cwd_handle.lock().expect("cwd handle lock").clone();
        let original_cwd = cwd_handle.read().expect("cwd lock").clone();
        *cwd_handle.write().expect("cwd lock") = created.worktree_path.clone();
        // Record the event (H2: the record lands now so the log is
        // reproducible; resume consumption is deferred).
        if let Err(e) = self
            .store
            .append(SessionLogEntry {
                id: EventId::new(),
                session: self.session_id,
                ts: 0,
                prev_hash: None,
                event: SessionEvent::WorktreeEnter {
                    slug: slug.clone(),
                    path: created.worktree_path.to_string_lossy().into_owned(),
                    branch: created.worktree_branch.clone(),
                    head_commit: created.head_commit.clone(),
                },
            })
            .await
        {
            tracing::warn!("worktree enter event append failed: {e}");
        }
        let session = WorktreeSession {
            original_cwd,
            worktree_path: created.worktree_path.clone(),
            worktree_name: slug.clone(),
            worktree_branch: Some(created.worktree_branch.clone()),
            original_branch,
            original_head_commit: created.head_commit,
            session_id: self.session_id,
        };
        *self.session.lock().expect("session lock") = Some(session);
        self.fire_worktree_event(HookEventKind::WorktreeCreate, created.worktree_path.clone())
            .await;
        let message = format!(
            "Created worktree at {} on branch {}. The session is now working in the worktree. Use exit_worktree to leave.",
            created.worktree_path.to_string_lossy(),
            created.worktree_branch,
        );
        Ok(EnterResult {
            worktree_path: created.worktree_path.to_string_lossy().into_owned(),
            worktree_branch: created.worktree_branch,
            message,
        })
    }

    /// Create a per-child worktree + narrow the fence, leaving the parent on
    /// its original cwd and wide fence. The returned guard travels with the
    /// child handle and is held for the child's run; Drop restores the wide
    /// fence on completion. Refused while a sandbox exec is in flight (an
    /// in-flight exec keeps its spawn-time profile and could escape the
    /// narrow). Fail-closed: a fence failure returns an error, never a
    /// degraded no-isolation spawn.
    pub async fn enter_for_child(&self, slug: String) -> Result<ChildWorktree, WorktreeError> {
        if self.sandbox.active_exec_count() > 0 {
            return Err(WorktreeError::Git {
                stderr: "per-child worktree refused: a sandbox exec is in flight".into(),
            });
        }
        let created = worktree_session::get_or_create_worktree(&self.repo_root, &slug)?;
        let guard = self
            .sandbox
            .narrow_to_worktree(&created.worktree_path, &self.git_common_dir)
            .map_err(|e| WorktreeError::Git {
                stderr: format!("narrow_to_worktree: {e}"),
            })?;
        // Register before the hook-fire await so an abort during that await
        // still leaves the tree in the reclaim map for the shutdown sweep. A
        // resumed tree carries a prior child's output; a diff against its
        // current tip would read that work as "no new changes" and let reclaim
        // delete it. Register only a freshly created tree for auto-reclaim: a
        // resumed one is the caller's to keep (an explicit exit removes it),
        // so it never enters the shutdown sweep.
        if !created.existed {
            self.pending_reclaims.lock().expect("reclaims lock").insert(
                slug.clone(),
                ReclaimSlot {
                    record: WorktreeReclaimRecord {
                        repo_root: self.repo_root.clone(),
                        worktree_path: created.worktree_path.clone(),
                        worktree_branch: created.worktree_branch.clone(),
                        slug: slug.clone(),
                        head_commit: created.head_commit.clone().unwrap_or_default(),
                    },
                    completion: ReclaimCompletion::new(),
                    launched: false,
                },
            );
        }
        self.fire_worktree_event(HookEventKind::WorktreeCreate, created.worktree_path.clone())
            .await;
        Ok(ChildWorktree {
            worktree_path: created.worktree_path,
            worktree_branch: created.worktree_branch,
            slug,
            fence_guard: guard,
        })
    }

    /// Clean up a per-child worktree at terminal state: restore the wide
    /// fence, then remove the tree when the child left no changes or preserve
    /// it (with path + branch) when it did. Infallible: an abandoned tree is a
    /// cleanup outcome the caller observes, not an error to propagate at
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
        } = cw;
        if let Err(e) = fence_guard.restore() {
            tracing::warn!("child worktree fence restore failed: {e}");
        }
        // Launch the blocking reclaim if it has not been launched already, then
        // await the shared completion. The slot stays in the map through the
        // await so an abort mid-reclaim leaves the launched task's completion
        // reachable for the shutdown sweep; the slot is removed only once the
        // outcome is observed.
        let outcome = {
            let completion = match self
                .pending_reclaims
                .lock()
                .expect("reclaims lock")
                .get_mut(&slug)
            {
                Some(slot) => {
                    launch_reclaim(slot);
                    Arc::clone(&slot.completion)
                }
                // No reclaim record: the entry was already swept by a shutdown
                // sweep. Report the tree kept so the caller does not delete it
                // a second time.
                None => {
                    return ChildCleanup::Kept {
                        worktree_path,
                        worktree_branch,
                    };
                }
            };
            completion.await_outcome().await
        };
        self.pending_reclaims
            .lock()
            .expect("reclaims lock")
            .remove(&slug);
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
    /// sweep awaits that launched task's completion rather than relaunching Git, so
    /// a blocked branch deletion (a Git ref lock held by another process) is
    /// waited out, not raced by a second directory re-check. A slot whose
    /// child never reached cleanup is launched here for the first time. The
    /// sweep removes a clean tree without firing WorktreeRemove: the abort
    /// already left the child's lifecycle event unbalanced, and the silent
    /// recovery does not manufacture a removal event for it.
    pub async fn reclaim_abandoned(&self) {
        // Launch every not-yet-launched reclaim, collecting each completion.
        // The slots stay in the map through the await (a late cleanup_child
        // for the same slug would await the same completion rather than find
        // the slot drained); the map is cleared only once every outcome is
        // observed.
        let completions: Vec<Arc<ReclaimCompletion>> = self
            .pending_reclaims
            .lock()
            .expect("reclaims lock")
            .values_mut()
            .map(|slot| {
                launch_reclaim(slot);
                Arc::clone(&slot.completion)
            })
            .collect();
        for completion in completions {
            completion.await_outcome().await;
        }
        self.pending_reclaims.lock().expect("reclaims lock").clear();
    }

    /// Exit the worktree session (ExitWorktree tool body). keep preserves the
    /// worktree + branch; remove deletes both. remove with uncommitted changes
    /// + discard_changes=false refuses (fail-closed) + lists the work.
    #[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
    pub async fn exit(
        &self,
        action: ExitAction,
        discard_changes: bool,
    ) -> Result<ExitOutcome, WorktreeError> {
        let session = self.session.lock().expect("session lock").clone();
        let Some(session) = session else {
            // Scope guard: no-op when no EnterWorktree ran this session.
            return Ok(ExitOutcome {
                action,
                original_cwd: self.repo_root.to_string_lossy().into_owned(),
                worktree_path: String::new(),
                worktree_branch: None,
                message: "No active worktree session to exit.".into(),
            });
        };
        // Fail-closed discard gate (remove only). keep preserves regardless.
        if action == ExitAction::Remove && !discard_changes {
            if let Some(summary) = worktree_session::count_worktree_changes(
                &session.worktree_path,
                session.original_head_commit.as_deref(),
            ) {
                if summary.changed_files > 0 || summary.commits > 0 {
                    let mut parts = Vec::new();
                    if summary.changed_files > 0 {
                        let p = if summary.changed_files == 1 {
                            "file"
                        } else {
                            "files"
                        };
                        parts.push(format!("{} uncommitted {}", summary.changed_files, p));
                    }
                    if summary.commits > 0 {
                        let p = if summary.commits == 1 {
                            "commit"
                        } else {
                            "commits"
                        };
                        parts.push(format!(
                            "{} {} on {}",
                            summary.commits,
                            p,
                            session
                                .worktree_branch
                                .as_deref()
                                .unwrap_or("the worktree branch")
                        ));
                    }
                    return Err(WorktreeError::Git {
                        stderr: format!(
                            "Worktree has {}. Removing will discard this work permanently. Confirm with the user, then re-invoke with discard_changes=true — or use action=keep to preserve.",
                            parts.join(" and ")
                        ),
                    });
                }
            } else {
                // Could not verify state (git failure or no baseline) ->
                // refuse without explicit confirmation (fail-closed).
                return Err(WorktreeError::Git {
                    stderr: format!(
                        "Could not verify worktree state at {}. Refusing to remove without explicit confirmation. Re-invoke with discard_changes=true to proceed, or action=keep to preserve.",
                        session.worktree_path.display()
                    ),
                });
            }
        }
        // Restore the fence to the repo root BEFORE any other action (so the
        // remove git ops run with the wide fence, which can reach the main
        // .git). The guard restore also clears the narrow state.
        if let Some(guard) = self.fence_guard.lock().expect("fence guard lock").take()
            && let Err(e) = guard.restore()
        {
            tracing::warn!("worktree fence restore failed: {e}");
        }
        // Switch the cwd back to the original.
        let cwd_handle = self.cwd_handle.lock().expect("cwd handle lock").clone();
        *cwd_handle.write().expect("cwd lock") = session.original_cwd.clone();
        let (action_str, removed): (&str, bool) = match action {
            ExitAction::Keep => ("keep", false),
            ExitAction::Remove => ("remove", true),
        };
        if removed {
            // remove: git worktree remove --force + git branch -D (the
            // discard gate already confirmed the user accepts the loss).
            worktree_session::remove_worktree(&self.repo_root, &session.worktree_name)?;
            self.fire_worktree_event(HookEventKind::WorktreeRemove, session.worktree_path.clone())
                .await;
        }
        // H5: warn (not block) if the main branch ref moved while isolated.
        if let (Some(orig), Some(branch)) = (
            self.original_main_ref
                .lock()
                .expect("main ref lock")
                .as_ref(),
            session.original_branch.as_deref(),
        ) && let Some(now) =
            worktree_session::rev_parse(&self.repo_root, &format!("refs/heads/{branch}"))
            && &now != orig
        {
            // A rewritten main branch is security-relevant and must reach the
            // user when a notice destination is installed.
            let notice = format!(
                "worktree: main branch {branch} moved while isolated ({orig} -> {now}); the agent may have rewritten history"
            );
            let handler = self.user_notice.lock().expect("user notice lock").clone();
            if let Some(handler) = handler {
                handler.handle(UserNoticeEvent { message: notice });
            } else {
                tracing::warn!("{notice}");
            }
        }
        // Record the exit event (H2).
        if let Err(e) = self
            .store
            .append(SessionLogEntry {
                id: EventId::new(),
                session: self.session_id,
                ts: 0,
                prev_hash: None,
                event: SessionEvent::WorktreeExit {
                    action: action_str.into(),
                    path: session.worktree_path.to_string_lossy().into_owned(),
                },
            })
            .await
        {
            tracing::warn!("worktree exit event append failed: {e}");
        }
        *self.session.lock().expect("session lock") = None;
        let message = if removed {
            format!(
                "Exited and removed worktree at{}. Session is now back in {}.",
                session
                    .worktree_branch
                    .as_ref()
                    .map(|b| format!(" on branch {b}"))
                    .unwrap_or_default(),
                session.original_cwd.display()
            )
        } else {
            format!(
                "Exited worktree. Work preserved at{}{}. Session is now back in {}.",
                session
                    .worktree_branch
                    .as_ref()
                    .map(|b| format!(" on branch {b}"))
                    .unwrap_or_default(),
                session.worktree_path.display(),
                session.original_cwd.display()
            )
        };
        Ok(ExitOutcome {
            action,
            original_cwd: session.original_cwd.to_string_lossy().into_owned(),
            worktree_path: session.worktree_path.to_string_lossy().into_owned(),
            worktree_branch: session.worktree_branch,
            message,
        })
    }
}

#[cfg(test)]
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
mod tests;
