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
