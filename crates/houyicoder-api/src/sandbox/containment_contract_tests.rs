use super::{
    BoundaryAccess, Containment, Coverage, SideEffect, WorktreeFenceGuard, boundary_grants_for,
    normalize_tool_path, path_args_for_boundary,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::Arc,
};

struct StubFence;

impl Containment for StubFence {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: vec![PathBuf::from("/ws")],
        }
    }

    fn would_block(&self, effect: SideEffect) -> Option<String> {
        match effect {
            SideEffect::Network => Some("egress is contained".into()),
            _ => None,
        }
    }
}

fn assert_dyn_safe(_: &dyn Containment) {}

#[test]
fn test_stub_reports_fenced_coverage() {
    let fence = StubFence;
    assert!(matches!(fence.coverage(), Coverage::Fenced { .. }));
}

#[test]
fn test_boundary_defaults_none_empty() {
    let fence = StubFence;
    assert!(fence.boundary_root().is_none());
    assert!(fence.boundary_dirs().is_empty());
}

#[test]
fn test_stub_blocks_network_only() {
    let fence = StubFence;
    assert!(fence.would_block(SideEffect::Network).is_some());
    assert!(fence.would_block(SideEffect::None).is_none());
}

#[test]
fn test_stub_is_dyn_safe() {
    assert_dyn_safe(&StubFence);
}

/// An existing path canonicalizes directly.
#[test]
fn test_normalize_existing() {
    let dir = env::temp_dir();
    let canonical = normalize_tool_path(&dir).expect("temp dir resolves");
    assert!(canonical.is_absolute());
}

/// A not-yet-existing file under an existing directory resolves through
/// the parent: the directory part is canonical, the file name is
/// appended as-is.
#[test]
fn test_normalize_new_file() {
    let dir = env::temp_dir();
    let target = dir.join("houyicoder-no-such-file-xyz.txt");
    let canonical = normalize_tool_path(&target).expect("parent resolves");
    assert!(canonical.ends_with("houyicoder-no-such-file-xyz.txt"));
    let canonical_dir = dunce::canonicalize(&dir).expect("temp dir resolves");
    assert_eq!(
        canonical.parent(),
        Some(canonical_dir.as_path()),
        "the directory part resolves through the parent"
    );
}

/// A path with multiple missing trailing segments resolves through the
/// nearest existing ancestor, keeping the missing tail verbatim.
#[test]
fn test_normalize_missing_tail() {
    let dir = env::temp_dir();
    let tail = Path::new("houyicoder-missing-a")
        .join("missing-b")
        .join("missing-c.txt");
    let target = dir.join(&tail);
    let canonical = normalize_tool_path(&target).expect("ancestor resolves");
    assert!(canonical.ends_with(&tail));
}

/// A path under a nonexistent top-level directory still resolves: the walk
/// stops at the filesystem root, which anchors the missing segments.
#[test]
fn test_normalize_missing_root() {
    let missing_root = if cfg!(windows) {
        let prefix = env::temp_dir()
            .components()
            .next()
            .expect("drive prefix")
            .as_os_str()
            .to_os_string();
        PathBuf::from(prefix).join("houyicoder-no-such-root-xyz")
    } else {
        Path::new("/").join("houyicoder-no-such-root-xyz")
    };
    let target = missing_root.join("a").join("b.txt");
    let canonical = normalize_tool_path(&target).expect("root ancestor resolves");
    assert!(canonical.ends_with(Path::new("a").join("b.txt")));
}

/// An unresolved path containing parent components remains outside the
/// proven boundary and therefore requires a grant instead of passing through.
#[test]
fn test_unresolved_parent_requires_grant() {
    let base = env::temp_dir().join(format!("boundary-parent-{}", process::id()));
    let root = base.join("workspace");
    fs::create_dir_all(&root).expect("mkdir root");
    let input = json!({"path": "missing/../../outside/new.txt"});

    let grants = boundary_grants_for("write", Some(&input), &root, &[], &[]);

    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].access, BoundaryAccess::ReadWrite);
    fs::remove_dir_all(base).ok();
}

#[test]
fn test_file_tools_expose_path() {
    let input = json!({"path": "/outside/settings.json"});
    for tool in ["write", "edit", "multiedit"] {
        assert_eq!(
            path_args_for_boundary(tool, Some(&input)),
            vec!["/outside/settings.json"],
            "{tool} must expose its path to the approval boundary"
        );
    }
    assert!(
        path_args_for_boundary("read", Some(&input)).is_empty(),
        "a read approval must not widen a directory's write fence"
    );
}

/// The fence guard runs the restore closure on explicit restore(), and
/// again on Drop (best-effort) — the second call is a no-op. A guard
/// dropped WITHOUT explicit restore also runs the closure (best-effort
/// path). Covers the Drop branch + the idempotent second-call path.
#[test]
fn test_fence_guard_restore_drop() {
    let count = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&count);
    let guard = WorktreeFenceGuard::new(Box::new(move || {
        c.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));
    assert!(guard.restore().is_ok());
    assert_eq!(count.load(Ordering::SeqCst), 1, "explicit restore ran once");
    drop(guard); // Drop best-effort — closure already taken, no-op.
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "drop after restore is a no-op"
    );

    // A guard dropped WITHOUT restore: Drop runs the closure.
    let c2 = Arc::clone(&count);
    let guard2 = WorktreeFenceGuard::new(Box::new(move || {
        c2.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));
    drop(guard2);
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "drop without restore runs the closure"
    );
}
