//! The worktree fence: a session moved into one checkout of its repository.
//!
//! The session narrows to a linked tree only when that tree belongs to this
//! repository, which Git proves with the pointer file it writes into the
//! checkout. The guard undoes the grants when it is restored.

use houyicoder_api::sandbox::{SandboxSession, WorktreeFenceGuard};
use houyicoder_context::SandboxError;
use std::fs::read_to_string;
use std::path::Path;
use std::sync::Arc;

use super::MacSeatbeltSession;

pub(super) fn narrow_to_worktree(
    session: &MacSeatbeltSession,
    worktree: &Path,
    git_common_dir: &Path,
) -> Result<WorktreeFenceGuard, SandboxError> {
    // Refuse while a running exec keeps its profile over the wider fence
    // alive: a backgrounded command could still write the main tree after the
    // fence moved. The caller should wait or cancel first.
    if session.active_exec_count() > 0 {
        return Err(SandboxError::Unsupported(
            "worktree enter refused: a sandboxed command is still running".into(),
        ));
    }
    let worktree = dunce::canonicalize(worktree)
        .map_err(|e| SandboxError::Io(format!("worktree canonicalize: {e}")))?;
    let git_common = dunce::canonicalize(git_common_dir)
        .map_err(|e| SandboxError::Io(format!("git common canonicalize: {e}")))?;
    // The worktree must belong to this repository: a tree inside the root
    // is the product's own, and a linked tree is usually elsewhere, where
    // the link Git wrote into this repository's common directory is the
    // proof. Any other outside path is an escape attempt.
    if !worktree.starts_with(&session.workspace) && !linked_worktree_of(&worktree, &git_common) {
        return Err(SandboxError::PathTraversal(format!(
            "worktree path escapes the repo root: {}",
            worktree.display()
        )));
    }
    if !worktree.is_dir() {
        return Err(SandboxError::PathTraversal(format!(
            "worktree path is not a directory: {}",
            worktree.display()
        )));
    }
    // Allow-back the repo .git so git ops can read/write objects + refs,
    // and keep the main checkout readable while writes stay narrowed.
    session.add_working_dir(git_common.to_str().expect("git common path str"))?;
    session.add_reading_dir(session.workspace.to_str().expect("workspace path str"))?;
    // Record the narrow state: current_profile re-renders bound to the
    // worktree + appends the .git/config read-allow; exec uses the
    // worktree as cwd and leaves the environment untouched.
    *session
        .narrow_workspace
        .lock()
        .expect("narrow workspace lock") = Some(worktree.clone());
    *session
        .narrow_git_common
        .lock()
        .expect("narrow git common lock") = Some(git_common.clone());
    // The restore closure reverts the narrow state + drops the .git
    // allow-back. It captures Arc clones of the shared Mutex fields so it
    // can revert through the original session (the Arc-clone points at the
    // same Mutex). Runs on explicit restore or Drop.
    let narrow_ws = Arc::clone(&session.narrow_workspace);
    let narrow_git = Arc::clone(&session.narrow_git_common);
    let dirs = session.dirs.clone();
    let git_common_for_restore = git_common.clone();
    let repo_for_restore = session.workspace.clone();
    Ok(WorktreeFenceGuard::new(Box::new(move || {
        *narrow_ws.lock().expect("restore: narrow ws lock") = None;
        *narrow_git.lock().expect("restore: narrow git lock") = None;
        dirs.remove_write(&git_common_for_restore);
        dirs.remove_read(&repo_for_restore);
        Ok(())
    })))
}

/// Whether the directory is a linked worktree of the repository whose common
/// directory is given. Git records the link in a .git file that points at a
/// directory under the common worktrees directory, which is what shows the
/// directory is a checkout of this repository rather than any other path
/// outside the checkout.
fn linked_worktree_of(worktree: &Path, git_common: &Path) -> bool {
    let Ok(pointer) = read_to_string(worktree.join(".git")) else {
        return false;
    };
    let Some(target) = pointer.trim().strip_prefix("gitdir:") else {
        return false;
    };
    let Ok(admin) = dunce::canonicalize(worktree.join(target.trim())) else {
        return false;
    };
    admin.starts_with(git_common.join("worktrees"))
}
