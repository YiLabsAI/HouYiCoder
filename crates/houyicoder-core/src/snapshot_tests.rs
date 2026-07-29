//! Tests for snapshot + undo: the LIFO undo stack, reflink policy,
//! workspace size caps, and restore round-trip.

use super::*;

#[test]
fn test_undo_stack_lifo() {
    let mut s = UndoStack::new();
    assert!(s.is_empty());
    s.push(UndoEntry::BeforeImage {
        path: PathBuf::from("a"),
        before: Some(b"x".to_vec()),
    });
    s.push(UndoEntry::BeforeImage {
        path: PathBuf::from("b"),
        before: None,
    });
    assert_eq!(s.len(), 2);
    assert!(s.pop().is_some());
    assert_eq!(s.len(), 1);
    assert!(s.pop().is_some());
    assert!(s.is_empty());
}

#[test]
fn test_snapshot_restore_recovers_file() {
    let tmp = std::env::temp_dir().join(format!("snap-test-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::create_dir_all(&tmp).expect("mkdir");
    std::fs::write(tmp.join("hello.txt"), "world").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    let entry = store.snapshot(&[]).expect("snapshot");
    // Mutate the file (simulates a destructive op).
    std::fs::write(tmp.join("hello.txt"), "DELETED").expect("mutate");
    assert_eq!(
        std::fs::read_to_string(tmp.join("hello.txt")).unwrap(),
        "DELETED"
    );
    // Undo restores.
    store.restore(&entry).expect("restore");
    assert_eq!(
        std::fs::read_to_string(tmp.join("hello.txt")).unwrap(),
        "world"
    );
    std::fs::remove_dir_all(&tmp).ok();
}

/// A resumed session re-links its undo stack to the on-disk snapshots the
/// prior process left: the in-memory stack is gone on restart, but the
/// snap-N dirs persist. relink_undo_entries reconstructs LIFO entries
/// pointing at them, so /undo still works across a restart. The crash-
/// after-destructive-op case (the gap this closes) is the intersection
/// this test pins.
#[test]
fn test_relink_recovers_snapshots_restart() {
    let tmp = std::env::temp_dir().join(format!("snap-relink-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::create_dir_all(&tmp).expect("mkdir");
    std::fs::write(tmp.join("hello.txt"), "world").expect("write");
    // Prior process: take a snapshot (simulates a destructive bash guard).
    let store = SnapshotStore::new(&tmp).expect("store");
    let entry = store.snapshot(&[]).expect("snapshot");
    let UndoEntry::CoWSnapshot {
        store_path: entry_path,
        ..
    } = &entry
    else {
        panic!("expected CoWSnapshot");
    };
    std::fs::write(tmp.join("hello.txt"), "DELETED").expect("mutate");
    // Restart: a fresh store has an empty in-memory stack, but the on-disk
    // snap-N dir survives.
    let resumed = SnapshotStore::new(&tmp).expect("resumed store");
    let mut stack = UndoStack::from_entries(resumed.relink_undo_entries());
    let restored = stack.pop().expect("re-linked entry");
    let UndoEntry::CoWSnapshot {
        store_path: restored_path,
        ..
    } = &restored
    else {
        panic!("expected CoWSnapshot");
    };
    assert_eq!(
        restored_path, entry_path,
        "re-linked the surviving snapshot"
    );
    resumed.restore(&restored).expect("restore after relink");
    assert_eq!(
        std::fs::read_to_string(tmp.join("hello.txt")).unwrap(),
        "world",
        "undo works after a restart via re-link"
    );
    std::fs::remove_dir_all(&tmp).ok();
}

/// relink_undo_entries skips non-directory entries, non-snap-N named dirs,
/// and non-numeric ids -- only valid workspace-only snap-N dirs re-link.
/// Pins the skip branches so a regression that re-links junk goes red.
#[test]
fn test_relink_skips_junk_entries() {
    let tmp = std::env::temp_dir().join(format!("snap-relink-junk-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    let store = SnapshotStore::new(&tmp).expect("store");
    // A valid workspace-only snapshot (the one entry that should re-link).
    let entry = store.snapshot(&[]).expect("snapshot");
    let UndoEntry::CoWSnapshot {
        store_path: valid, ..
    } = entry
    else {
        panic!("expected CoWSnapshot");
    };
    let store_dir = tmp.join(".houyicoder").join("snapshots");
    // A junk file in the store dir (skip: not a directory).
    std::fs::write(store_dir.join("junk-file"), "x").unwrap();
    // A non-snap-N named directory (skip: name does not match snap-<n>).
    std::fs::create_dir_all(store_dir.join("other-dir")).unwrap();
    // Re-link from a fresh store (simulates restart).
    let resumed = SnapshotStore::new(&tmp).expect("resumed");
    let entries = resumed.relink_undo_entries();
    assert_eq!(
        entries.len(),
        1,
        "only the valid snapshot re-links: {entries:?}"
    );
    let UndoEntry::CoWSnapshot { store_path, .. } = &entries[0] else {
        panic!("expected CoWSnapshot");
    };
    assert_eq!(store_path, &valid, "re-linked the valid snapshot");
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn test_snapshot_prunes_store_target() {
    let tmp = std::env::temp_dir().join(format!("snap-prune-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::create_dir_all(tmp.join("target")).expect("mkdir target");
    std::fs::write(tmp.join("target/build.txt"), "x").expect("write target");
    std::fs::write(tmp.join("real.txt"), "y").expect("write real");
    let store = SnapshotStore::new(&tmp).expect("store");
    let entry = store.snapshot(&[]).expect("snapshot");
    let UndoEntry::CoWSnapshot { store_path, .. } = entry else {
        panic!("expected CoWSnapshot");
    };
    // The snapshot must NOT contain target/ (pruned) or the snapshot
    // store dir itself (pruned). It must contain real.txt.
    assert!(!store_path.join("target").exists(), "target pruned");
    assert!(
        !store_path.join(".houyicoder").exists(),
        "snapshot store pruned from its own walk"
    );
    assert!(store_path.join("real.txt").exists(), "real.txt snapshotted");
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn test_destructive_command_detected() {
    assert!(is_destructive_command("rm -rf src/"));
    assert!(is_destructive_command("sudo apt install x"));
    assert!(is_destructive_command("echo hello > file.txt"));
    assert!(!is_destructive_command("ls -la"));
    assert!(!is_destructive_command("echo hello"));
    assert!(!is_destructive_command("grep -r foo ."));
    assert!(!is_destructive_command("echo 'a > b'"));
}

#[test]
fn test_undo_entry_description() {
    let cow = UndoEntry::CoWSnapshot {
        store_path: PathBuf::from("/tmp/snap-0"),
        extra_roots: vec![],
    };
    assert!(cow.description().contains("restored"));
    assert!(cow.description().contains("snapshot"));
    let img = UndoEntry::BeforeImage {
        path: PathBuf::from("/tmp/f.txt"),
        before: Some(b"x".to_vec()),
    };
    assert!(img.description().contains("restored"));
}

/// A minimal stub SandboxSession for the BashTool hook test (canned
/// exec that returns success without actually running anything).
struct StubSession {
    root: PathBuf,
}
impl houyicoder_api::sandbox::SandboxSession for StubSession {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: houyicoder_context::sandbox_types::ExecConfig,
    ) -> houyicoder_async::PFut<
        '_,
        Result<
            houyicoder_context::sandbox_types::ExecResult,
            houyicoder_context::sandbox_types::SandboxError,
        >,
    > {
        Box::pin(async move {
            Ok(houyicoder_context::sandbox_types::ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }
    fn read_file(
        &self,
        _path: &str,
        _max_bytes: usize,
    ) -> houyicoder_async::PFut<'_, Result<Vec<u8>, houyicoder_context::sandbox_types::SandboxError>>
    {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn write_file(
        &self,
        _path: &str,
        _content: Vec<u8>,
    ) -> houyicoder_async::PFut<'_, Result<(), houyicoder_context::sandbox_types::SandboxError>>
    {
        Box::pin(async move { Ok(()) })
    }
    fn list_dir(
        &self,
        _path: &str,
    ) -> houyicoder_async::PFut<
        '_,
        Result<
            Vec<houyicoder_context::sandbox_types::DirEntry>,
            houyicoder_context::sandbox_types::SandboxError,
        >,
    > {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn path_exists(
        &self,
        _path: &str,
    ) -> houyicoder_async::PFut<'_, Result<bool, houyicoder_context::sandbox_types::SandboxError>>
    {
        Box::pin(async move { Ok(false) })
    }
    fn workspace_root(&self) -> std::sync::Arc<std::path::Path> {
        std::sync::Arc::from(self.root.clone())
    }
}

#[test]
fn test_bash_tool_snapshots_destructive() {
    use crate::agent::BashTool;
    use houyicoder_api::tool::{Tool, ToolCtx};
    use std::sync::Arc;
    let tmp = std::env::temp_dir().join(format!("bash-snap-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::create_dir_all(&tmp).expect("mkdir");
    std::fs::write(tmp.join("real.txt"), "data").expect("write");
    let store = Arc::new(SnapshotStore::new(&tmp).expect("store"));
    let stack = Arc::new(std::sync::Mutex::new(UndoStack::new()));
    let session = Arc::new(StubSession { root: tmp.clone() });
    let tool = BashTool::with_undo(session, stack.clone(), store);
    // Execute a destructive command — the hook snapshots + pushes.
    let input = serde_json::json!({"command": "rm -rf src/"});
    let result = pollster::block_on(tool.execute(ToolCtx::new("test"), input));
    assert!(result.is_ok(), "exec should succeed (stub returns 0)");
    assert_eq!(
        stack.lock().unwrap().len(),
        1,
        "undo stack should have 1 entry after a destructive command"
    );
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn test_workspace_size_sums_files() {
    let tmp = std::env::temp_dir().join(format!("ws-size-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::create_dir_all(&tmp).expect("mkdir");
    std::fs::write(tmp.join("a.txt"), b"hello").expect("write");
    std::fs::write(tmp.join("b.txt"), b"world!!").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    let size = store.workspace_size();
    assert!(
        size >= 12,
        "sums file sizes (at least the two files' bytes)"
    );
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn test_check_reflink_policy_passes() {
    // On APFS (the test runner's FS), reflink should be available, so the
    // policy passes regardless of workspace size.
    let tmp = std::env::temp_dir().join(format!("reflink-ok-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::create_dir_all(&tmp).expect("mkdir");
    let store = SnapshotStore::new(&tmp).expect("store");
    assert!(
        store.check_reflink_policy().is_ok(),
        "policy passes when reflink is available"
    );
    std::fs::remove_dir_all(&tmp).ok();
}
#[test]
fn test_reflink_available_policy_passes() {
    // APFS/btrfs/XFS path: reflink available -> Ok regardless of size.
    assert!(reflink_policy(true, 0).is_ok());
    assert!(reflink_policy(true, u64::MAX).is_ok());
}

#[test]
fn test_small_workspace_without_reflink() {
    // ext4 fallback: CoW unavailable but workspace under threshold -> Ok
    // (full-copy fallback is acceptable for small workspaces).
    assert!(reflink_policy(false, 0).is_ok());
    assert!(reflink_policy(false, NO_REFLINK_SIZE_THRESHOLD).is_ok());
}

#[test]
fn test_large_workspace_without_reflink() {
    // ext4 fallback: CoW unavailable + workspace over threshold -> Err so
    // the caller degrades to Ask instead of paying a slow full copy.
    let err = reflink_policy(false, NO_REFLINK_SIZE_THRESHOLD + 1).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    assert!(err.to_string().contains("degrade to Ask"));
}

#[test]
fn test_undo_stack_push_safe() {
    // Concurrent pushers under the shared Arc<Mutex<UndoStack>> must not
    // panic or corrupt the stack. Deterministic invariant: after all
    // threads join, len == total pushes (no lost entries).
    use std::sync::{Arc, Mutex};
    use std::thread;
    let stack = Arc::new(Mutex::new(UndoStack::new()));
    let n_threads = 8;
    let per_thread = 500;
    let mut handles = vec![];
    for t in 0..n_threads {
        let s = stack.clone();
        handles.push(thread::spawn(move || {
            for i in 0..per_thread {
                s.lock().unwrap().push(UndoEntry::BeforeImage {
                    path: std::path::PathBuf::from(format!("t{t}-{i}")),
                    before: None,
                });
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(stack.lock().unwrap().len(), n_threads * per_thread);
}

#[test]
fn test_undo_stack_pop_drains() {
    // Pre-fill, then concurrent poppers drain the stack. Deterministic
    // invariant: every pop that returns Some subtracts one, so total
    // popped == total pushed and the stack ends empty.
    use std::sync::{Arc, Mutex};
    use std::thread;
    let stack = Arc::new(Mutex::new(UndoStack::new()));
    let total = 2000;
    for i in 0..total {
        stack.lock().unwrap().push(UndoEntry::BeforeImage {
            path: std::path::PathBuf::from(format!("f{i}")),
            before: None,
        });
    }
    let n_threads = 8;
    let mut handles = vec![];
    for _ in 0..n_threads {
        let s = stack.clone();
        handles.push(thread::spawn(move || {
            let mut popped = 0usize;
            while s.lock().unwrap().pop().is_some() {
                popped += 1;
            }
            popped
        }));
    }
    let mut total_popped = 0usize;
    for h in handles {
        total_popped += h.join().unwrap();
    }
    assert_eq!(total_popped, total);
    assert_eq!(stack.lock().unwrap().len(), 0);
}

#[test]
fn test_audit_sink_records_lifecycle() {
    // The injected audit sink receives snapshot_created / snapshot_pruned /
    // undo_applied as each lifecycle event happens. Guards the wiring that
    // threads the audit sink through the snapshot lifecycle.
    use std::sync::{Arc, Mutex};
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    struct Sink(Arc<Mutex<Vec<String>>>);
    impl SnapshotAuditSink for Sink {
        fn snapshot_created(&self, _: &UndoEntry) {
            self.0.lock().unwrap().push("created".into());
        }
        fn snapshot_pruned(&self, n: usize) {
            self.0.lock().unwrap().push(format!("pruned:{n}"));
        }
        fn undo_applied(&self, _: &UndoEntry) {
            self.0.lock().unwrap().push("undo".into());
        }
    }
    let tmp = std::env::temp_dir().join(format!("audit-sink-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("f.txt"), "x").unwrap();
    let store = SnapshotStore::new(&tmp)
        .unwrap()
        .with_audit(Arc::new(Sink(events.clone())));
    let entry = store.snapshot(&[]).unwrap();
    store.snapshot(&[]).unwrap();
    // Restore before pruning so the snapshot still exists on disk.
    store.restore(&entry).unwrap();
    // size cap 0 prunes everything (the two snapshots are not protected).
    store.prune(604800, 0, &[]);
    let ev = events.lock().unwrap();
    assert_eq!(
        ev.iter().filter(|s| s.as_str() == "created").count(),
        2,
        "two snapshots created: {ev:?}"
    );
    assert!(
        ev.iter().any(|s| s.as_str().starts_with("pruned:")),
        "pruned fired: {ev:?}"
    );
    assert!(
        ev.iter().any(|s| s.as_str() == "undo"),
        "undo applied fired: {ev:?}"
    );
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn test_prune_removes_old_snapshots() {
    let tmp = std::env::temp_dir().join(format!("prune-old-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::create_dir_all(&tmp).expect("mkdir");
    std::fs::write(tmp.join("real.txt"), "data").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    store.snapshot(&[]).expect("snap1");
    store.snapshot(&[]).expect("snap2");
    // Prune with size cap 0 (remove everything) + no protected entries.
    let removed = store.prune(604800, 0, &[]);
    assert!(removed >= 1, "size cap 0 removes at least one snapshot");
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn test_prune_skips_protected_snapshots() {
    let tmp = std::env::temp_dir().join(format!("prune-prot-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::create_dir_all(&tmp).expect("mkdir");
    std::fs::write(tmp.join("real.txt"), "data").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    let entry = store.snapshot(&[]).expect("snap");
    let protected = match &entry {
        UndoEntry::CoWSnapshot { store_path, .. } => vec![store_path.clone()],
        _ => vec![],
    };
    // Prune with size cap 0 but the snapshot is protected → 0 removed.
    let removed = store.prune(604800, 0, &protected);
    assert_eq!(removed, 0, "protected snapshot not pruned");
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn test_undo_stack_peek_paths() {
    let mut s = UndoStack::new();
    assert!(s.peek().is_none());
    assert!(s.snapshot_paths().is_empty());
    s.push(UndoEntry::CoWSnapshot {
        store_path: PathBuf::from("/tmp/snap-0"),
        extra_roots: vec![],
    });
    s.push(UndoEntry::BeforeImage {
        path: PathBuf::from("a"),
        before: None,
    });
    assert!(s.peek().is_some());
    let paths = s.snapshot_paths();
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("snap-0"));
}

#[test]
fn test_id_continues_across_restart() {
    let tmp = std::env::temp_dir().join(format!(
        "snap-id-{}-{}",
        std::process::id(),
        std::sync::atomic::AtomicU64::new(0).fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::create_dir_all(&tmp).expect("mkdir");
    std::fs::write(tmp.join("real.txt"), "data").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    store.snapshot(&[]).expect("snap-0");
    store.snapshot(&[]).expect("snap-1");
    // Simulate restart: new store on the same dir. The counter must
    // pick up past the existing snapshots to avoid collision — the new
    // snapshot id must be >= 2 (not snap-0 again).
    let store2 = SnapshotStore::new(&tmp).expect("store2");
    let entry = store2.snapshot(&[]).expect("snap");
    let path = match &entry {
        UndoEntry::CoWSnapshot { store_path, .. } => store_path,
        _ => unreachable!(),
    };
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let id = name
        .strip_prefix("snap-")
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0);
    assert!(id >= 2, "counter continues past restart: id={id}");
    std::fs::remove_dir_all(&tmp).ok();
}
