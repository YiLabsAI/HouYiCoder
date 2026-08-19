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
