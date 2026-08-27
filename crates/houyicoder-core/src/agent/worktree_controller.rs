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
//! Both enter and exit append a TurnEvent (WorktreeEnter / WorktreeExit) to
//! the session log so a replay can restore the execution environment later —
//! the record lands now (resume consumption is a separate, deferred task);
//! never silent on the cwd + fence switch.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use houyicoder_api::hook_fire::HookFire;
use houyicoder_api::live::{LiveEvent, LiveSink};
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::session::SessionLog;
use houyicoder_context::{
    EventId, HookEventKind, HookFirePayload, SessionId, TurnEvent, TurnEventKind,
};

use super::worktree_session::{self, WorktreeError, WorktreeSession};

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
pub struct ChildWorktree {
    pub worktree_path: PathBuf,
    pub worktree_branch: String,
    slug: String,
    head_commit: String,
    pub fence_guard: houyicoder_api::sandbox::WorktreeFenceGuard,
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
    fence_guard: Mutex<Option<houyicoder_api::sandbox::WorktreeFenceGuard>>,
    store: Arc<dyn SessionLog>,
    session_id: SessionId,
    /// The main branch ref at enter, so exit can warn (not block) if the
    /// sandboxed agent rewrote it. The fence allows .git refs, so a malicious
    /// or buggy agent could move main; the alert keeps it from being silent.
    original_main_ref: Mutex<Option<String>>,
    /// Optional live sink for user-visible notices (the main-branch-moved
    /// alert). Injected after construction, same staged-delegation pattern
    /// as set_cwd_handle: the controller is built before the runner, then
    /// the composition root attaches the runner's sink once it exists.
    live: Mutex<Option<LiveSink>>,
    /// The hook-fire seam for WorktreeCreate / WorktreeRemove events.
    /// Attached after construction (staged delegation, like set_live_sink):
    /// the controller is built before the runner, so the composition root
    /// attaches the runner's hook-fire handle once it exists. None when no
    /// hooks are wired; fire is a no-op then.
    hook_fire: Mutex<Option<Arc<dyn HookFire>>>,
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
            live: Mutex::new(None),
            hook_fire: Mutex::new(None),
        }
    }

    /// Swap in the runner's real cwd handle. Call after the runner is built
    /// so enter/exit writes reach the runner's ContextBuilder (the single
    /// source of truth the next system-prompt build reads).
    pub fn set_cwd_handle(&self, cwd: Arc<RwLock<PathBuf>>) {
        *self.cwd_handle.lock().expect("cwd handle lock") = cwd;
    }

    /// Attach the runner's live sink for user-visible notices (the
    /// main-branch-moved alert). Same staged-delegation pattern as
    /// set_cwd_handle: the controller is built before the runner.
    pub fn set_live_sink(&self, sink: LiveSink) {
        *self.live.lock().expect("live sink lock") = Some(sink);
    }

    /// Attach the hook-fire seam for WorktreeCreate / WorktreeRemove events.
    /// Staged delegation, like set_live_sink: the controller is built before
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
            format!("wt-{}-{}", std::process::id(), n)
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
            .append(TurnEvent {
                id: EventId::new(),
                session: self.session_id,
                ts: 0,
                prev_hash: None,
                kind: TurnEventKind::WorktreeEnter {
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
        self.fire_worktree_event(HookEventKind::WorktreeCreate, created.worktree_path.clone())
            .await;
        Ok(ChildWorktree {
            worktree_path: created.worktree_path,
            worktree_branch: created.worktree_branch,
            slug,
            head_commit: created.head_commit.unwrap_or_default(),
            fence_guard: guard,
        })
    }

    /// Clean up a per-child worktree at terminal state: restore the wide
    /// fence, then auto-remove the worktree when the child left no changes or
    /// preserve it (with path + branch) when it did. A dirty worktree is kept
    /// so the caller can continue on the branch; a clean one is deleted. The
    /// diff check is fail-closed: a git failure preserves the worktree rather
    /// than risking destroying real work.
    pub async fn cleanup_child(&self, cw: ChildWorktree) -> Result<ChildCleanup, WorktreeError> {
        let path = cw.worktree_path.clone();
        let branch = cw.worktree_branch.clone();
        let slug = cw.slug.clone();
        let head = cw.head_commit.clone();
        // Drop the guard -- restores the wide fence (Drop is the fallback;
        // the explicit drop runs it now, before the git remove).
        drop(cw);
        if worktree_session::has_worktree_changes(&path, &head) {
            return Ok(ChildCleanup::Kept {
                worktree_path: path,
                worktree_branch: branch,
            });
        }
        worktree_session::remove_worktree(&self.repo_root, &slug)?;
        self.fire_worktree_event(HookEventKind::WorktreeRemove, path.clone())
            .await;
        Ok(ChildCleanup::Removed {
            worktree_path: path,
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
            // The main branch moved while the agent was isolated — it may
            // have rewritten history. This is security-relevant: the user
            // must see it, not just the diagnostic log. Surface through the
            // live sink as a system line; fall back to tracing when no sink
            // is attached (tests, the stub path).
            let notice = format!(
                "worktree: main branch {branch} moved while isolated ({orig} -> {now}); the agent may have rewritten history"
            );
            if let Some(sink) = self.live.lock().expect("live sink lock").as_ref() {
                sink(&LiveEvent::SystemLine { text: notice });
            } else {
                tracing::warn!("{notice}");
            }
        }
        // Record the exit event (H2).
        if let Err(e) = self
            .store
            .append(TurnEvent {
                id: EventId::new(),
                session: self.session_id,
                ts: 0,
                prev_hash: None,
                kind: TurnEventKind::WorktreeExit {
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
#[path = "worktree_controller_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "worktree_child_tests.rs"]
mod child_tests;
