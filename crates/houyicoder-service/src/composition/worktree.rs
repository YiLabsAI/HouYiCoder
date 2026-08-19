//! Worktree-related composition helpers, split from composition.rs so that
//! file stays under the file-size gate. Holds the worktree controller wiring
//! (build + register the enter/exit tools) + the canonical git-root slug
//! derivation (shared across linked worktrees so the auto-scope memory dir is
//! the same regardless of which worktree is active).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::session::SessionLog;
use houyicoder_context::SessionId;
use houyicoder_core::agent::{
    EnterWorktreeTool, ExitWorktreeTool, ToolRegistry, WorktreeController,
};
use houyicoder_permission::{GuardedTool, ModeGate};
use houyicoder_session::SessionStore;

/// Build the worktree controller when both a workspace + a sandbox session
/// resolved, and register the enter/exit tools through the permission gate.
/// None when no sandbox session resolved (no isolation -> no worktree tools).
/// The controller's cwd handle starts as a dummy; the composition root calls
/// set_cwd_handle on it once the runner is built so enter/exit writes reach
/// the runner's ContextBuilder.
pub fn wire_worktree_controller(
    workspace: Option<&Path>,
    sandbox_session: Option<&Arc<dyn SandboxSession>>,
    store: &Arc<SessionStore>,
    session: SessionId,
    tools: &mut ToolRegistry,
    gate_dyn: &Arc<dyn ModeGate>,
) -> Option<Arc<WorktreeController>> {
    let (ws, sb) = (workspace?, sandbox_session?);
    let store_for_controller: Arc<dyn SessionLog> = store.clone();
    let controller = Arc::new(WorktreeController::new(
        ws.to_path_buf(),
        git_common_dir(ws).unwrap_or_else(|| ws.join(".git")),
        Arc::clone(sb),
        store_for_controller,
        session,
    ));
    tools.register(Arc::new(GuardedTool::new(
        Arc::new(EnterWorktreeTool::new(controller.clone())),
        gate_dyn.clone(),
    )));
    tools.register(Arc::new(GuardedTool::new(
        Arc::new(ExitWorktreeTool::new(controller.clone())),
        gate_dyn.clone(),
    )));
    Some(controller)
}

/// The canonical .git common dir for a workspace: git rev-parse
/// --git-common-dir, canonicalized. For a linked worktree this resolves
/// the .git gitfile indirection to the main repo's shared .git dir (where
/// config, objects, refs live); for a main repo it is the repo's own .git.
/// None when git is unavailable or the path cannot be canonicalized.
///
/// This is the path the worktree controller allow-backs into the narrow
/// fence so a linked worktree can read the main repo's .git/config and the
/// worktrees metadata dir. Passing the raw workspace .git here instead
/// would, for a linked worktree, hand the fence the gitfile (a text
/// pointer) rather than the common dir, so the allow-back would target a
/// non-existent path and git log would fail. Host-side git probing (not a
/// sandboxed tool run), so the spawn-chokepoint rule is allowed here.
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
pub fn git_common_dir(ws: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(ws)
        .arg("rev-parse")
        .arg("--git-common-dir")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return None;
    }
    std::fs::canonicalize(ws.join(&s))
        .or_else(|_| std::fs::canonicalize(std::path::Path::new(&s)))
        .ok()
}

/// The canonical git-root slug for a workspace, shared across linked
/// worktrees so the auto-scope memory dir is the same regardless of which
/// worktree is active. Falls back to the workspace's own dir name when git
/// is unavailable. Host-side git probing (not a sandboxed tool run), so the
/// spawn-chokepoint rule is allowed here.
pub fn git_canonical_slug(ws: &Path) -> String {
    let common = git_common_dir(ws);
    common
        .as_deref()
        .and_then(|c| c.parent())
        .and_then(|root| root.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| canonical_dir_name(ws))
}

fn canonical_dir_name(p: &Path) -> String {
    std::fs::canonicalize(p)
        .ok()
        .and_then(|c| c.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "default".to_string())
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "test setup spawn, not model-driven"
)]
mod tests {
    use super::*;
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_permission::DefaultModeGate;

    /// The slug tests need a real repo, so they skip when git is absent.
    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    /// A repo whose dir name is exactly the given name, under a pid- and
    /// seq-unique parent so parallel runs do not collide. One empty commit, so a
    /// linked worktree has a HEAD to branch from.
    fn temp_git_repo(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let parent =
            std::env::temp_dir().join(format!("houyi-slug-{}-{}", std::process::id(), seq));
        drop(std::fs::remove_dir_all(&parent));
        std::fs::create_dir_all(&parent).expect("mkdir parent");
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir repo");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .arg("init")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git init");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--allow-empty",
                "-m",
                "x",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git commit");
        dir
    }

    /// None when git refuses, so the caller skips rather than fails.
    fn add_linked_worktree(main: &Path, name: &str) -> Option<PathBuf> {
        let wt = main
            .parent()
            .expect("parent")
            .join(format!("{name}-{}", std::process::id()));
        let added = std::process::Command::new("git")
            .arg("-C")
            .arg(main)
            .args(["worktree", "add", "--detach"])
            .arg(&wt)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok();
        if added { Some(wt) } else { None }
    }

    /// Remove the linked worktree and the whole temp parent dir.
    fn cleanup(main: &Path, wt: &Path) {
        drop(
            std::process::Command::new("git")
                .arg("-C")
                .arg(main)
                .args(["worktree", "remove", "--force"])
                .arg(wt)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status(),
        );
        drop(std::fs::remove_dir_all(main.parent().expect("parent")));
    }

    /// A main repo's slug is its own dir name (the git common dir's parent).
    #[test]
    fn test_slug_matches_repo_name() {
        if !git_available() {
            return;
        }
        let repo = temp_git_repo("mainrepo");
        assert_eq!(git_canonical_slug(&repo), "mainrepo");
        drop(std::fs::remove_dir_all(repo.parent().expect("parent")));
    }

    /// A linked worktree must resolve to the same slug as its main repo so a
    /// project keeps one memory dir. Deriving the slug from the workspace dir
    /// name would split memory across every worktree the user creates.
    #[test]
    fn test_linked_shares_slug() {
        if !git_available() {
            return;
        }
        let main = temp_git_repo("sharedmain");
        let Some(wt) = add_linked_worktree(&main, "sharedwt") else {
            drop(std::fs::remove_dir_all(main.parent().expect("parent")));
            return;
        };
        let main_slug = git_canonical_slug(&main);
        assert_eq!(
            main_slug,
            git_canonical_slug(&wt),
            "worktree must share the main repo slug"
        );
        assert_eq!(main_slug, "sharedmain");
        cleanup(&main, &wt);
    }

    /// A linked worktree's .git is a gitfile, not a directory. The fence must
    /// allow-back the real shared .git, so the lookup follows that indirection;
    /// allow-backing the gitfile would leave git commands failing in the
    /// worktree session.
    #[test]
    fn test_common_dir_resolves_gitfile() {
        if !git_available() {
            return;
        }
        let main = temp_git_repo("sharedmain2");
        let Some(wt) = add_linked_worktree(&main, "sharedwt2") else {
            drop(std::fs::remove_dir_all(main.parent().expect("parent")));
            return;
        };
        let main_git = std::fs::canonicalize(main.join(".git")).expect("canonicalize main .git");
        let wt_common = git_common_dir(&wt).expect("common dir resolves for linked worktree");
        assert_eq!(
            std::fs::canonicalize(&wt_common).expect("canonicalize wt common"),
            main_git,
            "linked worktree common dir must be the main repo .git, not the gitfile"
        );
        assert!(
            main_git.is_dir(),
            "resolved common dir must be a directory, not the gitfile"
        );
        assert!(
            !wt.join(".git").is_dir(),
            "linked worktree .git is a gitfile, not a directory"
        );
        cleanup(&main, &wt);
    }

    /// A non-git dir falls back to its own canonical dir name, so memory still
    /// works outside a repo (just not shared across worktrees).
    #[test]
    fn test_slug_non_git_dir() {
        let dir = std::env::temp_dir().join(format!("houyi-slug-nogit-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("mkdir non-git");
        assert_eq!(
            git_canonical_slug(&dir),
            format!("houyi-slug-nogit-{}", std::process::id())
        );
        drop(std::fs::remove_dir_all(&dir));
    }

    /// The enter/exit tools exist only when there is something to isolate: a
    /// workspace AND a sandbox session. With either missing nothing registers,
    /// so the model is never offered a switch it cannot perform.
    #[test]
    fn test_controller_registers_tools() {
        let dir = std::env::temp_dir().join(format!("houyi-wire-wt-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let sandbox: Arc<dyn SandboxSession> =
            Arc::new(houyicoder_sandbox::PlatformSession::new_in_cwd(&dir).expect("sandbox"));
        let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());

        let mut tools = ToolRegistry::new();
        let wired = wire_worktree_controller(
            Some(dir.as_path()),
            Some(&sandbox),
            &store,
            SessionId::new(),
            &mut tools,
            &gate,
        );
        assert!(wired.is_some(), "controller built when both resolved");
        assert!(
            tools.get("enter_worktree").is_some(),
            "enter_worktree registered"
        );
        assert!(
            tools.get("exit_worktree").is_some(),
            "exit_worktree registered"
        );

        let mut bare = ToolRegistry::new();
        let unwired =
            wire_worktree_controller(None, None, &store, SessionId::new(), &mut bare, &gate);
        assert!(unwired.is_none(), "None when no workspace or sandbox");
        assert!(
            bare.get("enter_worktree").is_none(),
            "no tools registered when unwired"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
