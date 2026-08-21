//! Containment-bridge utilities split from the composition root so it stays
//! under the file-size gate. The adapter + directory rehydration are the seam
//! between a SandboxSession (the fence owner) and the Containment /
//! RuleStore traits the composition root threads.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use houyicoder_api::sandbox::{Containment, Coverage, SandboxSession, SideEffect};
use houyicoder_permission::RuleStore;

/// Adapter that delegates Containment to a SandboxSession's
/// as_containment method. Works around the double-trait-object
/// problem: Arc<dyn SandboxSession> cannot be cast to
/// Arc<dyn Containment> even when the concrete type implements both.
pub(crate) struct ContainmentAdapter(pub Arc<dyn SandboxSession>);

impl Containment for ContainmentAdapter {
    fn coverage(&self) -> Coverage {
        self.0
            .as_containment()
            .map(|c| c.coverage())
            .unwrap_or(Coverage::Unfenced)
    }
    fn would_block(&self, effect: SideEffect) -> Option<String> {
        self.0.as_containment().and_then(|c| c.would_block(effect))
    }
    fn boundary_root(&self) -> Option<Arc<Path>> {
        // The adapter wraps a SandboxSession; workspace_root is on SandboxSession
        // (not Containment), so delegate to the session directly. coverage() used
        // as_containment() because coverage is a fence property the session may
        // not own; root is a session property. Return Some whenever the session
        // reports a root (the gate's path-bounds check uses it; None means the
        // gate degrades to "do not ask", letting confine_path enforce).
        Some(self.0.workspace_root())
    }
    fn boundary_dirs(&self) -> Vec<PathBuf> {
        self.0
            .working_dirs()
            .into_iter()
            .map(PathBuf::from)
            .collect()
    }
}

/// Rehydrate persistent directory authorizations from the rule store into the
/// kernel fence. Directories the user persisted (via /permissions AddDir or an
/// approval card) live in the store's envelope; the fence is in-memory and
/// starts empty, so without this bridge a persistent directory auth is silent
/// on restart — the store has it, but the kernel fence does not, and the tool
/// still refuses. Errors are ignored: a directory deleted since it was
/// persisted should not brick startup; the stale entry just does not re-attach.
pub(crate) fn rehydrate_directories(session: &dyn SandboxSession, store: &dyn RuleStore) {
    let dirs = store.load_directories();
    let mut failed = 0;
    for dir in &dirs {
        if session.add_working_dir(&dir.to_string_lossy()).is_err() {
            failed += 1;
        }
    }
    if failed > 0 {
        tracing::warn!(
            "startup: {failed}/{} persistent directory authorizations failed to re-attach to the fence; the corresponding tools will refuse those paths",
            dirs.len()
        );
    }
}

/// Allow-back the repo git common dir when the workspace is a linked
/// worktree, so git can write there.
///
/// A linked worktree holds a .git FILE pointing at the main repo's shared
/// .git dir, and that is where git actually writes: the index lives at
/// <common>/worktrees/<name>/index, and every commit writes objects and refs
/// under <common>. The fence's write allow-back covers the workspace, and for
/// a linked worktree the common dir sits OUTSIDE it, so without this bridge
/// every git write fails -- add, commit, and stash all abort with
/// "Unable to create <common>/worktrees/<name>/index.lock: Operation not
/// permitted". The enter-worktree path already allow-backs the common dir
/// when it narrows the fence; a session that simply STARTS with its cwd
/// inside an existing worktree never went through that path, so it was left
/// unable to commit.
///
/// A main repo needs nothing: its common dir is <workspace>/.git, already
/// inside the workspace allow-back, so the call is skipped. The write denies
/// that matter survive either way -- hooks and config stay write-denied by
/// the mandatory deny segment, which lands after every allow-back.
pub(crate) fn attach_git_common_dir(session: &dyn SandboxSession, workspace: &Path) {
    let Some(common) = super::worktree::git_common_dir(workspace) else {
        return;
    };
    if common.starts_with(workspace) {
        return;
    }
    if let Err(e) = session.add_working_dir(&common.to_string_lossy()) {
        tracing::warn!(
            "startup: could not allow-back the git common dir {}: {e}; git writes from this worktree will be refused by the fence",
            common.display()
        );
    }
}

// macOS-only: every test here widens a live fence, which only Seatbelt
// supports. Landlock is irreversible once applied and a Job Object carries no
// path fence, so both correctly report add_working_dir as Unsupported.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use houyicoder_permission::{FileRuleStore, Scope};
    use houyicoder_sandbox::PlatformSession;

    /// Never default_paths: that would write the developer's real home.
    fn temp_store(root: &Path) -> Arc<dyn RuleStore> {
        Arc::new(FileRuleStore::new(
            root.join("user.json"),
            root.join("project.json"),
            root.join("local.json"),
        ))
    }

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("{tag}-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&root));
        std::fs::create_dir_all(&root).expect("mkdir root");
        root
    }

    /// The grant is durable but the fence is in-memory and starts empty, so
    /// startup has to carry it across. Without that the store still lists the
    /// directory while the fence does not, and the tool refuses a path the user
    /// already approved.
    #[test]
    fn test_startup_restores_fence() {
        let root = temp_root("houyi-rehydrate");
        let store = temp_store(&root);
        let target = root.join("authorized-dir");
        std::fs::create_dir_all(&target).expect("mkdir target");
        store
            .add_directory(&target, Scope::Project)
            .expect("add_directory");
        let canonical = std::fs::canonicalize(&target).expect("canonicalize target");

        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let session: Arc<dyn SandboxSession> =
            Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
        assert!(
            session.working_dirs().is_empty(),
            "fence starts empty before the restore"
        );

        rehydrate_directories(session.as_ref(), store.as_ref());
        let dirs = session.working_dirs();
        assert!(
            dirs.iter().any(|d| Path::new(d.as_str()) == canonical),
            "the persisted directory must be back in the fence: {dirs:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A directory deleted since it was persisted must not brick startup. The
    /// stale entry is skipped and the still-valid one re-attaches, so one bad
    /// entry in the store cannot cost the user every other grant.
    #[test]
    fn test_startup_skips_stale_dir() {
        let root = temp_root("houyi-rehydrate-stale");
        let store = temp_store(&root);
        let target = root.join("authorized-dir");
        std::fs::create_dir_all(&target).expect("mkdir target");
        store
            .add_directory(&target, Scope::Project)
            .expect("add_directory");
        let canonical = std::fs::canonicalize(&target).expect("canonicalize target");
        // Never created, so its re-attach fails.
        store
            .add_directory(&root.join("stale-deleted"), Scope::Project)
            .expect("add stale dir");

        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let session: Arc<dyn SandboxSession> =
            Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
        rehydrate_directories(session.as_ref(), store.as_ref());

        let dirs = session.working_dirs();
        assert!(
            dirs.iter().any(|d| Path::new(d.as_str()) == canonical),
            "the valid directory still re-attaches: {dirs:?}"
        );
        assert!(
            !dirs.iter().any(|d| d.contains("stale-deleted")),
            "the stale directory must not re-attach: {dirs:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Init a git repo with one commit, and link a worktree under it. Returns
    /// None when git is unavailable or any step fails, so the caller skips
    /// rather than failing on a missing tool.
    #[expect(clippy::disallowed_methods, reason = "test fixture, not model-driven")]
    fn git_repo_with_worktree(root: &Path) -> Option<(PathBuf, PathBuf)> {
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).ok()?;
        let git = |args: &[&str], cwd: &Path| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .ok()
                .filter(|o| o.status.success())
        };
        git(&["init"], &repo)?;
        git(&["config", "user.email", "t@t"], &repo)?;
        git(&["config", "user.name", "t"], &repo)?;
        std::fs::write(repo.join("f.txt"), b"x").ok()?;
        git(&["add", "."], &repo)?;
        git(&["commit", "-m", "init"], &repo)?;
        let wt = root.join("wt");
        git(&["worktree", "add", wt.to_str()?], &repo)?;
        Some((repo, wt))
    }

    /// A session whose workspace IS a linked worktree must get the main repo
    /// git dir into the fence. That dir holds the worktree's index, plus the
    /// objects and refs a commit writes, and it sits outside the worktree, so
    /// without the allow-back git cannot write at all -- add, commit and stash
    /// all abort on index.lock. The enter-worktree path allow-backs it when it
    /// narrows; a session that merely STARTS inside an existing worktree never
    /// goes through that path.
    #[test]
    fn test_worktree_attaches_git_common() {
        let root = temp_root("houyi-wt-gitcommon");
        let Some((repo, wt)) = git_repo_with_worktree(&root) else {
            std::fs::remove_dir_all(&root).ok();
            return;
        };
        let wt_canon = std::fs::canonicalize(&wt).expect("canonicalize worktree");
        let session: Arc<dyn SandboxSession> =
            Arc::new(PlatformSession::new_in_cwd(&wt_canon).expect("sandbox"));
        assert!(
            session.working_dirs().is_empty(),
            "fence starts with no extra dirs"
        );

        attach_git_common_dir(session.as_ref(), &wt_canon);

        let main_git = std::fs::canonicalize(repo.join(".git")).expect("canonicalize main .git");
        let dirs = session.working_dirs();
        assert!(
            dirs.iter().any(|d| Path::new(d.as_str()) == main_git),
            "the main repo git dir must be in the fence so git can write from the worktree: {dirs:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A main repo needs no allow-back: its git dir is inside the workspace,
    /// already covered. Adding it anyway would widen nothing but would show up
    /// as a working dir in /permissions, telling the user the fence was
    /// extended when it was not.
    #[test]
    fn test_main_repo_attaches_nothing() {
        let root = temp_root("houyi-main-gitcommon");
        let Some((repo, _wt)) = git_repo_with_worktree(&root) else {
            std::fs::remove_dir_all(&root).ok();
            return;
        };
        let repo_canon = std::fs::canonicalize(&repo).expect("canonicalize repo");
        let session: Arc<dyn SandboxSession> =
            Arc::new(PlatformSession::new_in_cwd(&repo_canon).expect("sandbox"));

        attach_git_common_dir(session.as_ref(), &repo_canon);

        assert!(
            session.working_dirs().is_empty(),
            "a main repo's git dir is already inside the workspace: {:?}",
            session.working_dirs()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The gate asks through Containment instead of holding the session, so the
    /// query must report the bounds the session enforces: workspace root plus
    /// runtime-added dirs. Drop either and the gate sees a smaller fence than
    /// the kernel does, refusing paths that are in fact allowed.
    #[test]
    fn test_containment_reports_bounds() {
        let root = temp_root("adapter-bound");
        let extra = root.join("extra");
        std::fs::create_dir_all(&extra).expect("mkdir extra");
        let session: Arc<dyn SandboxSession> =
            Arc::new(PlatformSession::new_in_cwd(&root).expect("sandbox"));
        session
            .add_working_dir(&extra.to_string_lossy())
            .expect("widen the fence");

        let adapter = ContainmentAdapter(session);
        assert_eq!(
            adapter.boundary_root().map(|p| p.to_path_buf()),
            Some(std::fs::canonicalize(&root).expect("canonicalize root")),
            "the root the session enforces must be the root the gate sees"
        );
        let dirs = adapter.boundary_dirs();
        let widened = std::fs::canonicalize(&extra).expect("canonicalize extra");
        assert!(
            dirs.iter().any(|d| d == &widened),
            "a runtime-added dir must be in the bounds the gate sees: {dirs:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
