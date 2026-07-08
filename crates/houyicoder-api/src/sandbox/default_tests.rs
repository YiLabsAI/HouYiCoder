//! Tests for the SandboxSession trait DEFAULT impls (resolve + file ops).
//!
//! The tests module's Stub overrides the file ops with canned responses,
//! so it does not exercise the defaults. FsStub here inherits the defaults
//! against a real temp dir, covering the default bodies' branches (roundtrip,
//! max_bytes truncation, traversal reject, list/exists, nested-dir create).
use super::*;

struct FsStub {
    ws: PathBuf,
}
impl SandboxSession for FsStub {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        unreachable!("FsStub exercises the file-op defaults, not exec")
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(self.ws.clone())
    }
}

fn fs_stub() -> FsStub {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let raw = std::env::temp_dir().join(format!("houyi-api-fsstub-{}-{}", std::process::id(), n));
    std::fs::create_dir_all(&raw).expect("create stub workspace");
    // Canonicalize: the default resolve compares a canonicalized path against
    // workspace_root(), so workspace_root() must itself be canonical (real
    // backends canonicalize at construction; /var on macOS is a symlink to
    // /private/var, so a raw temp_dir() path would falsely fail starts_with).
    let ws = dunce::canonicalize(&raw).unwrap_or(raw);
    FsStub { ws }
}

#[tokio::test]
async fn test_default_file_ops_roundtrip() {
    let stub = fs_stub();
    let ws = stub.ws.clone();
    stub.write_file("a.txt", vec![1, 2, 3]).await.unwrap();
    assert_eq!(stub.read_file("a.txt", 100).await.unwrap(), vec![1, 2, 3]);
    // max_bytes truncation branch
    assert_eq!(stub.read_file("a.txt", 2).await.unwrap(), vec![1, 2]);
    // create_dir_all branch (parent does not yet exist)
    stub.write_file("sub/b.txt", vec![9]).await.unwrap();
    assert_eq!(stub.read_file("sub/b.txt", 100).await.unwrap(), vec![9]);
    let entries = stub.list_dir(".").await.unwrap();
    assert!(entries.iter().any(|e| e.name == "a.txt"));
    assert!(stub.path_exists("a.txt").await.unwrap());
    assert!(!stub.path_exists("nope.txt").await.unwrap());
    std::fs::remove_dir_all(&ws).ok();
}

#[tokio::test]
async fn test_resolve_fallback_blocks_traversal() {
    // Two shapes of .. traversal, both must fail to read an outside file.
    let stub = fs_stub();
    let ws = stub.ws.clone();
    // An outside file in the workspace's parent (reachable only via ..).
    let outside = ws
        .parent()
        .expect("workspace has a parent")
        .join("outside_marker");
    std::fs::write(&outside, b"secret").unwrap();
    // A real sub-dir so sub/../.. has a resolvable OS prefix.
    stub.write_file("sub/x", vec![1]).await.unwrap();

    // sub/../../outside_marker: the parent exists (sub does), so canonicalize
    // resolves the .. to the workspace's parent; the canonical result no
    // longer starts_with(workspace) -> PathTraversal.
    let err = stub
        .read_file("sub/../../outside_marker", 100)
        .await
        .unwrap_err();
    assert!(
        matches!(err, SandboxError::PathTraversal(_)),
        "traversal via an existing prefix must be caught: {err:?}"
    );

    // nope/../../outside_marker: the parent does not exist (nope missing), so
    // resolve falls back to the non-canonical joined path; starts_with passes
    // literally, but the OS read cannot traverse the nonexistent prefix ->
    // Err (never the secret). This locks that the fallback cannot be coerced
    // into reading the outside file.
    let res = stub.read_file("nope/../../outside_marker", 100).await;
    assert!(
        res.is_err(),
        "nonexistent-prefix traversal must not read the outside file: {res:?}"
    );

    std::fs::remove_dir_all(&ws).ok();
    std::fs::remove_file(&outside).ok();
}

#[tokio::test]
async fn test_resolve_rejects_traversal() {
    let stub = fs_stub();
    let ws = stub.ws.clone();
    // absolute path -> resolve PathTraversal -> read_file early-returns Err
    let err = stub.read_file("/etc/passwd", 10).await.unwrap_err();
    assert!(
        matches!(err, SandboxError::PathTraversal(_)),
        "absolute rejected: {err:?}"
    );
    // .. escape -> canonical lands outside the workspace -> PathTraversal
    let err = stub.read_file("../escape.txt", 10).await.unwrap_err();
    assert!(
        matches!(err, SandboxError::PathTraversal(_)),
        "escape rejected: {err:?}"
    );
    std::fs::remove_dir_all(&ws).ok();
}
