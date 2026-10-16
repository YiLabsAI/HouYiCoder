//! The containment layer: the sandbox session and permission gate a run is
//! fenced by, the port view the gate asks for path bounds through, and the
//! bridges that carry persistent directory grants and the git common dir into
//! the kernel fence.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::degrade_with_notice;
use super::resolve_project_workspace;
use super::worktree::git_common_dir;
use crate::sandbox_policy::{PolicyWarning, network_policy_from};
use houyicoder_api::sandbox::{Containment, Coverage, SandboxSession, SideEffect};
use houyicoder_permission::{DefaultModeGate, RuleStore};
use houyicoder_resilience::resource_breaker::{ResourceBreaker, ResourceBreakerConfig};
use houyicoder_sandbox::PlatformSession;

/// The fences and the permission gate one session runs under.
pub(super) struct SessionFences {
    pub(super) workspace: Option<PathBuf>,
    pub(super) sandbox_session: Option<Arc<dyn SandboxSession>>,
    pub(super) breaker: Arc<ResourceBreaker>,
    pub(super) network_warnings: Vec<PolicyWarning>,
    pub(super) gate: Arc<DefaultModeGate>,
}

/// Build the session's fences and permission gate.
///
/// The fence is the single authority over containment; the gate delegates
/// out-of-workspace path checks to it via ContainmentAdapter.
pub(super) fn build_session_fences(
    project: Option<String>,
    rule_store: Option<Arc<dyn RuleStore>>,
) -> SessionFences {
    let mut gate_builder = DefaultModeGate::new();
    if let Some(rule_store) = &rule_store {
        gate_builder = gate_builder.with_store(rule_store.clone());
    }
    let (network, network_warnings) =
        network_policy_from(&houyicoder_config::load_sandbox_network());
    let breaker = Arc::new(ResourceBreaker::new(ResourceBreakerConfig::default()));
    let workspace = resolve_project_workspace(project);
    let fence = |s: PlatformSession| {
        s.with_breaker(breaker.clone())
            .with_network(network.clone())
    };
    let tempdir_session = || {
        degrade_with_notice(
            PlatformSession::new(),
            "sandbox unavailable: no isolated tempdir session could be created",
            "the agent starts with no filesystem or command tools.",
        )
        .map(&fence)
    };
    let sandbox = match &workspace {
        Some(ws) => degrade_with_notice(
            PlatformSession::new_in_cwd(ws),
            &format!("sandbox could not pin to workspace {ws:?}"),
            "falling back to an isolated tempdir -- the agent's bash will NOT see your repo.",
        )
        .map(&fence)
        .or_else(tempdir_session),
        None => {
            tracing::warn!(
                "no project root found (no Cargo.toml walking up from cwd, and HOUYICODER_PROJECT unset); sandboxing to an isolated tempdir -- the agent's bash will NOT see your repo. run from the repo root or pass --project <path>."
            );
            tempdir_session()
        }
    };
    let sandbox_session: Option<Arc<dyn SandboxSession>> =
        sandbox.map(|s| Arc::new(s) as Arc<dyn SandboxSession>);
    if let (Some(session), Some(store)) = (&sandbox_session, &rule_store) {
        restore_directory_grants(session.as_ref(), store.as_ref());
    }
    if let (Some(session), Some(ws)) = (&sandbox_session, workspace.as_deref()) {
        attach_git_common_dir(session.as_ref(), ws);
    }
    if let Some(ref session) = sandbox_session {
        gate_builder = gate_builder.with_containment(Arc::new(ContainmentAdapter(session.clone())));
    }
    SessionFences {
        workspace,
        sandbox_session,
        breaker,
        network_warnings,
        gate: Arc::new(gate_builder),
    }
}

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

    fn boundary_write_dirs(&self) -> Vec<PathBuf> {
        self.0
            .writing_dirs()
            .into_iter()
            .map(PathBuf::from)
            .collect()
    }
}

/// Restore persistent directory grants from the rule store into the kernel
/// fence. Directories the user persisted (via /permissions AddDir or an
/// approval card) live in the store's envelope; the fence is in-memory and
/// starts empty, so without this bridge a persistent directory auth is silent
/// on restart — the store has it, but the kernel fence does not, and the tool
/// still refuses. Errors are ignored: a directory deleted since it was
/// persisted should not brick startup; the stale entry just does not re-attach.
pub(crate) fn restore_directory_grants(session: &dyn SandboxSession, store: &dyn RuleStore) {
    let write_dirs = store.load_directories();
    let read_dirs = store.load_read_directories();
    let mut failed = 0;
    for dir in &write_dirs {
        if session.add_working_dir(&dir.to_string_lossy()).is_err() {
            failed += 1;
        }
    }
    for dir in &read_dirs {
        if session.add_reading_dir(&dir.to_string_lossy()).is_err() {
            failed += 1;
        }
    }
    let total = write_dirs.len() + read_dirs.len();
    if failed > 0 {
        tracing::warn!(
            "startup: {failed}/{total} persistent directory authorizations failed to re-attach to the fence; the corresponding tools will refuse those paths"
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
    // Normalize before anything consumes the path, not just before the
    // comparison: the git probe and the containment check must see the same
    // plain form. Production workspaces arrive from the resolver in dunce
    // form, but a std-canonicalized caller would on Windows carry the
    // verbatim prefix, which never prefix-matches the plain-form common dir;
    // git for Windows is also widely reported to reject it, which would fail
    // the probe and leave the allow-back a silent no-op.
    let ws = dunce::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let Some(common) = git_common_dir(&ws) else {
        return;
    };
    if common.starts_with(&ws) {
        return;
    }
    if let Err(e) = session.add_working_dir(&common.to_string_lossy()) {
        tracing::warn!(
            "startup: could not allow-back the git common dir {}: {e}; git writes from this worktree will be refused by the fence",
            common.display()
        );
    }
}

// Every backend admits runtime grants now: macOS re-renders the seatbelt
// profile at the next spawn, Linux hands the grant set to the fence helper
// in argv, and Windows widens the resolver (its job object carries no path
// primitive, so the grant never claims kernel backing). The restore and
// allow-back flows tested here therefore run on every platform, and path
// comparisons go through dunce on both sides.

#[cfg(test)]
mod tests;
