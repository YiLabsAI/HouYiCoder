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
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use self::child_reclaim::ReclaimCompletion;
use self::child_reclaim::ReclaimSlot;
use self::child_reclaim::WorktreeReclaimRecord;

use houyicoder_api::agent_event::{EventHandler, UserNoticeEvent};
use houyicoder_api::hook_fire::HookFire;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::session::SessionLog;
use houyicoder_context::{
    EventId, HookEventKind, HookFirePayload, SessionEvent, SessionId, SessionLogEntry,
};

use crate::agent::worktree::record::WorktreeLease;
use crate::agent::worktree::session as worktree_session;
use crate::agent::worktree::session::WorktreeError;
use crate::agent::worktree::session::WorktreeSession;
use houyicoder_api::sandbox::WorktreeFenceGuard;
use std::process::id;
use tokio::task::spawn_blocking;

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
    /// Held for the child's run and released before its tree is reclaimed, so
    /// the reclaim is not refused by the child's own hold and no other session
    /// enters the tree meanwhile.
    lease: WorktreeLease,
}

/// The worktree the session entered, with the lease that keeps other sessions
/// from working in the same tree. The lease is taken out of this owner before
/// a removal asks git to delete the tree, and released with the owner on keep.
struct ActiveWorktree {
    session: WorktreeSession,
    /// None only between the release before a removal and the exit that
    /// clears this owner.
    lease: Option<WorktreeLease>,
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

pub struct WorktreeController {
    repo_root: PathBuf,
    git_common_dir: PathBuf,
    session: Mutex<Option<ActiveWorktree>>,
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
        // The lease marks the tree as in use for the whole session, so a
        // removal preview or a second session sees it and keeps out. It is
        // taken before the tree exists: a tree created and then refused a lease
        // is a directory nothing owns, and the refusal would arrive only after
        // the cost of making it.
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
        *self.session.lock().expect("session lock") = Some(ActiveWorktree {
            session,
            lease: Some(lease),
        });
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
        // Register before the fence is restricted and before the hook-fire
        // await, so a failure or abort in either still leaves the tree in the
        // reclaim map for the shutdown sweep. A resumed tree carries a prior
        // child's output; a diff against its current tip would read that work
        // as no new changes and let reclaim delete it. Only a freshly created
        // tree is registered: a resumed one is the caller's to keep, so it
        // never enters the sweep.
        if !created.existed {
            self.pending_reclaims.lock().expect("reclaims lock").insert(
                slug.clone(),
                ReclaimSlot::new(WorktreeReclaimRecord {
                    repo_root: self.repo_root.clone(),
                    worktree_path: created.worktree_path.clone(),
                    worktree_branch: created.worktree_branch.clone(),
                    slug: slug.clone(),
                    head_commit: created.head_commit.clone(),
                }),
            );
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
            lease,
        } = cw;
        if let Err(e) = fence_guard.restore() {
            tracing::warn!("child worktree fence restore failed: {e}");
        }
        // The child is done with the tree, so its hold ends here: the reclaim
        // below refuses a leased tree, and this session's own lease is not a
        // reason to keep one.
        drop(lease);
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
                    slot.launch();
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
                slot.launch();
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
        let session = self
            .session
            .lock()
            .expect("session lock")
            .as_ref()
            .map(|active| active.session.clone());
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
        let removal_failed = if removed {
            // remove: git worktree remove --force + git branch -D (the
            // discard gate already confirmed the user accepts the loss). The
            // session's own lease is released first: removal refuses a leased
            // tree, and this session is the one ending, not one to keep out.
            let lease = self
                .session
                .lock()
                .expect("session lock")
                .as_mut()
                .and_then(|active| active.lease.take());
            drop(lease);
            // Removal walks a directory that may hold a build tree, so it runs
            // on the blocking pool rather than on an async worker; the reclaim
            // path runs the same act the same way.
            let repo_root = self.repo_root.clone();
            let slug = session.worktree_name.clone();
            let walk_path = session.worktree_path.clone();
            let removal =
                spawn_blocking(move || worktree_session::remove_worktree(&repo_root, &slug)).await;
            match removal {
                Ok(Ok(())) => {
                    self.fire_worktree_event(
                        HookEventKind::WorktreeRemove,
                        session.worktree_path.clone(),
                    )
                    .await;
                    None
                }
                Ok(Err(error)) => {
                    // The cwd and the fence already went back to the
                    // repository root and the lease is gone, so nothing here
                    // still describes a session inside the tree. What the
                    // failure leaves behind depends on the step it stopped at:
                    // before the directory is gone the tree stays whole, and a
                    // branch Git will not delete after the directory is gone
                    // leaves the branch and the record.
                    Some(error)
                }
                Err(error) => Some(WorktreeError::State {
                    path: walk_path,
                    reason: format!("the removal did not finish: {error}"),
                }),
            }
        } else {
            None
        };
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
        let recorded_action = recorded_exit_action(removal_failed.as_ref(), action_str);
        if let Err(e) = self
            .store
            .append(SessionLogEntry {
                id: EventId::new(),
                session: self.session_id,
                ts: 0,
                prev_hash: None,
                event: SessionEvent::WorktreeExit {
                    action: recorded_action.into(),
                    path: session.worktree_path.to_string_lossy().into_owned(),
                },
            })
            .await
        {
            tracing::warn!("worktree exit event append failed: {e}");
        }
        *self.session.lock().expect("session lock") = None;
        if let Some(error) = removal_failed {
            // The session is over whether or not the removal finished, and the
            // caller has to learn that from the same answer: a tool error that
            // only named the removal would leave a model believing it still
            // works inside a tree it has already left.
            return Err(WorktreeError::RemovalFailed {
                path: session.worktree_path.clone(),
                reason: error.to_string(),
            });
        }
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

/// The action an exit records: the state the session ended in rather than the
/// request, because a removal that stopped partway leaves something other than
/// the answer the caller asked for. It names the directory taken with the
/// branch kept, the registration gone with the directory and branch kept for a
/// later removal, and every other failure as a removal that did not finish.
/// That last value reports the request, not how much of the tree survived.
fn recorded_exit_action(
    removal_failed: Option<&WorktreeError>,
    requested: &'static str,
) -> &'static str {
    match removal_failed {
        None => requested,
        Some(WorktreeError::RemovalIncomplete { .. }) => "remove-incomplete",
        Some(WorktreeError::CleanupPending { .. }) => "cleanup-pending",
        Some(_) => "keep",
    }
}

mod child_reclaim;

#[cfg(test)]
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
mod tests;
