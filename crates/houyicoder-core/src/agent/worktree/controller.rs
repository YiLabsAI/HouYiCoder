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

use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

pub use self::child::ChildCleanup;
use self::child::ChildReclaimLedger;
pub use self::child::ChildWorktree;

use houyicoder_api::agent_event::{EventHandler, UserNoticeEvent};
use houyicoder_api::hook_fire::HookFire;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::session::SessionLog;
use houyicoder_context::{
    EventId, HookEventKind, HookFirePayload, SessionEvent, SessionId, SessionLogEntry,
};

use crate::agent::worktree::preview::WorktreePreview;
use crate::agent::worktree::preview::preview_remove_worktree;
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

/// A worktree the controller has already entered as far as the non-awaiting
/// steps go: the tree exists, the fence and the working directory have moved,
/// and the session holds its lease. Handed to finish_enter for the log record
/// and the hook, so the caller can do the Git work on the blocking pool
/// first.
#[derive(Debug)]
pub struct PreparedEnter {
    slug: String,
    path: PathBuf,
    branch: String,
    head_commit: Option<String>,
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

/// The worktree the session entered, with the lease that keeps other sessions
/// from working in the same tree. The lease is taken out of this owner before
/// a removal asks git to delete the tree, and released with the owner on keep.
struct ActiveWorktree {
    session: WorktreeSession,
    /// None only between the release before a removal and the exit that
    /// clears this owner.
    lease: Option<WorktreeLease>,
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
    /// The per-child trees this controller created and has not yet reclaimed.
    /// The ledger keeps each tree's recovery record reachable after the handle
    /// that owns it is gone, so a child aborted mid-cleanup still has its tree
    /// reclaimed off the worker.
    reclaims: ChildReclaimLedger,
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
            reclaims: ChildReclaimLedger::default(),
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

    /// What removing a managed worktree would discard, and the token naming
    /// that state. Nothing is written and nothing is deleted.
    ///
    /// The repository this controller was built for is the one asked about, so
    /// a caller names a slug rather than a path it composed itself.
    pub fn preview_remove(&self, slug: &str) -> Result<WorktreePreview, WorktreeError> {
        preview_remove_worktree(&self.repo_root, slug)
    }

    /// The slug of the managed worktree a directory names, for a caller that
    /// has a path rather than a name.
    pub fn slug_for_path(&self, path: &Path) -> Result<String, WorktreeError> {
        worktree_session::managed_slug_for_path(&self.repo_root, path)
    }

    /// Remove a worktree the user confirmed from its preview.
    ///
    /// The tree this session works in is refused before its lease is tried:
    /// the session's own hold would otherwise fail the removal's acquisition,
    /// which reads as another process using the tree rather than as the one
    /// thing the user cannot remove from inside it.
    pub fn remove_previewed(
        &self,
        slug: &str,
        token: &str,
        delete_unmerged_branch: bool,
    ) -> Result<(), WorktreeError> {
        let active = self.session.lock().expect("session lock");
        if let Some(active) = active.as_ref()
            && active.session.worktree_name == slug
        {
            return Err(WorktreeError::Leased {
                slug: slug.to_string(),
                reason: "this session is working in it".to_string(),
            });
        }
        drop(active);
        worktree_session::remove_previewed_worktree(
            &self.repo_root,
            slug,
            token,
            delete_unmerged_branch,
        )
    }

    /// Fire the removal hook for a tree the pane removed. The exit path fires
    /// the same event; a removal driven from the pane changes no session
    /// state, so the hook is the whole notice an automation gets.
    pub async fn fire_removed(&self, path: PathBuf) {
        self.fire_worktree_event(HookEventKind::WorktreeRemove, path)
            .await;
    }

    /// Enter a worktree session (EnterWorktree tool body). Creates or resumes
    /// the worktree, narrows the fence + cwd, records the main ref + a
    /// WorktreeEnter event. Refuses a second enter + refuses while an exec is
    /// in flight.
    pub async fn enter(&self, slug: Option<String>) -> Result<EnterResult, WorktreeError> {
        let prepared = self.enter_prepared(slug)?;
        self.finish_enter(prepared).await
    }

    /// The part of entering that does not await: the guard checks, the lease,
    /// the tree, the sandbox fence, the working directory swap, and the
    /// session install. Split from enter so a caller that keeps serving other
    /// work meanwhile can run this on the blocking pool; the Git checkout and
    /// the fence hold no awaiting point, and running them on the task that
    /// carries the session's events stalls that session until they finish.
    pub fn enter_prepared(&self, slug: Option<String>) -> Result<PreparedEnter, WorktreeError> {
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
        let session = WorktreeSession {
            original_cwd,
            worktree_path: created.worktree_path.clone(),
            worktree_name: slug.clone(),
            worktree_branch: Some(created.worktree_branch.clone()),
            original_branch,
            original_head_commit: created.head_commit.clone(),
            session_id: self.session_id,
        };
        *self.session.lock().expect("session lock") = Some(ActiveWorktree {
            session,
            lease: Some(lease),
        });
        Ok(PreparedEnter {
            slug,
            path: created.worktree_path,
            branch: created.worktree_branch,
            head_commit: created.head_commit,
        })
    }

    /// The part of entering that awaits: the session-log record, the
    /// worktree-create hook, and the answer.
    pub async fn finish_enter(
        &self,
        prepared: PreparedEnter,
    ) -> Result<EnterResult, WorktreeError> {
        // The record is written before the answer, so a log holds the enter
        // even when the answer never reaches the terminal.
        if let Err(e) = self
            .store
            .append(SessionLogEntry {
                id: EventId::new(),
                session: self.session_id,
                ts: 0,
                prev_hash: None,
                event: SessionEvent::WorktreeEnter {
                    slug: prepared.slug,
                    path: prepared.path.to_string_lossy().into_owned(),
                    branch: prepared.branch.clone(),
                    head_commit: prepared.head_commit,
                },
            })
            .await
        {
            tracing::warn!("worktree enter event append failed: {e}");
        }
        self.fire_worktree_event(HookEventKind::WorktreeCreate, prepared.path.clone())
            .await;
        let message = format!(
            "Created worktree at {} on branch {}. The session is now working in the worktree. Use exit_worktree to leave.",
            prepared.path.to_string_lossy(),
            prepared.branch,
        );
        Ok(EnterResult {
            worktree_path: prepared.path.to_string_lossy().into_owned(),
            worktree_branch: prepared.branch,
            message,
        })
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

mod child;

#[cfg(test)]
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
mod tests;
