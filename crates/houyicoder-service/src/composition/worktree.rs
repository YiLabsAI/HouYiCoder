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
