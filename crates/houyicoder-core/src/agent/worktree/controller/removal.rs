//! Removing a managed worktree by slug, for a tree no session is inside.

use std::path::PathBuf;

use super::WorktreeController;
use crate::agent::worktree::session as worktree_session;
use crate::agent::worktree::session::WorktreeError;
use crate::agent::worktree::session::canonicalize;
use houyicoder_context::HookEventKind;
use tokio::task::spawn_blocking;

impl WorktreeController {
    /// Remove a managed worktree by slug without a session in it.
    ///
    /// The session's own exit path can only take apart the tree the session
    /// works in, so a tree left behind by an earlier process — a resumed
    /// session holds no record of entering it — had no host-side removal at
    /// all, and the only route left was a shell removal, which the fence
    /// refuses wherever the tree carries a protected path. This takes the same
    /// managed path the exit uses: the creation record, then Git's own removal
    /// and branch deletion, all outside the fence.
    ///
    /// The tree the session currently works in is refused: it is removed by
    /// exiting it, so the session state cannot be left describing a directory
    /// that no longer exists.
    /// The removal is conservative unless the caller accepted the loss. A
    /// forced removal deletes uncommitted work and an unmerged branch, and the
    /// only thing that can authorize that is a caller saying so; a slug alone
    /// says which tree, not what may be destroyed. Without that acceptance
    /// Git's own refusals stand: a dirty tree and an unmerged branch both stop
    /// the removal instead of being thrown away.
    pub async fn remove_managed(
        &self,
        slug: &str,
        discard_changes: bool,
    ) -> Result<PathBuf, WorktreeError> {
        worktree_session::validate_worktree_slug(slug)?;
        let path = worktree_session::worktree_path_for(&self.repo_root, slug);
        {
            let active = self.session.lock().expect("session lock");
            if let Some(active) = active.as_ref()
                && canonicalize(&active.session.worktree_path).is_ok_and(|at| at == path)
            {
                return Err(WorktreeError::ActiveTree { path });
            }
        }
        let repo_root = self.repo_root.clone();
        let slug_owned = slug.to_string();
        spawn_blocking(move || {
            if discard_changes {
                worktree_session::remove_worktree(&repo_root, &slug_owned)
            } else {
                worktree_session::remove_clean_worktree(&repo_root, &slug_owned)
            }
        })
        .await
        .map_err(|e| WorktreeError::Git {
            stderr: format!("the removal task ended: {e}"),
        })??;
        self.fire_worktree_event(HookEventKind::WorktreeRemove, path.clone())
            .await;
        Ok(path)
    }
}
