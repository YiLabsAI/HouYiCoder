//! Tests for snapshot + undo: the LIFO undo stack, reflink policy,
//! workspace size caps, and restore round-trip.

use super::*;
use crate::agent::BashTool;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::tool::{Tool, ToolCtx};
use std::env::temp_dir;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::read_to_string;
use std::fs::remove_dir_all;
use std::fs::write;
use std::io::ErrorKind;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::thread;

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
    let tmp = temp_dir().join(format!("snap-test-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("hello.txt"), "world").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    let entry = store.snapshot(&[]).expect("snapshot");
    // Mutate the file (simulates a destructive op).
    write(tmp.join("hello.txt"), "DELETED").expect("mutate");
    assert_eq!(read_to_string(tmp.join("hello.txt")).unwrap(), "DELETED");
    // Undo restores.
    store.restore(&entry).expect("restore");
    assert_eq!(read_to_string(tmp.join("hello.txt")).unwrap(), "world");
    remove_dir_all(&tmp).ok();
}

/// A resumed session re-links its undo stack to the on-disk snapshots the
/// prior process left: the in-memory stack is gone on restart, but the
/// snap-N dirs persist. relink_undo_entries reconstructs LIFO entries
/// pointing at them, so /undo still works across a restart. The crash-
/// after-destructive-op case (the gap this closes) is the intersection
/// this test pins.
#[test]
fn test_relink_recovers_snapshots_restart() {
    let tmp = temp_dir().join(format!("snap-relink-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("hello.txt"), "world").expect("write");
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
    write(tmp.join("hello.txt"), "DELETED").expect("mutate");
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
        read_to_string(tmp.join("hello.txt")).unwrap(),
        "world",
        "undo works after a restart via re-link"
    );
    remove_dir_all(&tmp).ok();
}

/// relink_undo_entries skips non-directory entries, non-snap-N named dirs,
/// and non-numeric ids -- only valid workspace-only snap-N dirs re-link.
/// Pins the skip branches so a regression that re-links junk goes red.
#[test]
fn test_relink_skips_junk_entries() {
    let tmp = temp_dir().join(format!("snap-relink-junk-{}", id()));
    remove_dir_all(&tmp).ok();
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
    write(store_dir.join("junk-file"), "x").unwrap();
    // A non-snap-N named directory (skip: name does not match snap-<n>).
    create_dir_all(store_dir.join("other-dir")).unwrap();
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
    remove_dir_all(&tmp).ok();
}

#[test]
fn test_snapshot_prunes_store_target() {
    let tmp = temp_dir().join(format!("snap-prune-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(tmp.join("target")).expect("mkdir target");
    write(tmp.join("target/build.txt"), "x").expect("write target");
    write(tmp.join("real.txt"), "y").expect("write real");
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
    remove_dir_all(&tmp).ok();
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
impl SandboxSession for StubSession {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: houyicoder_context::ExecConfig,
    ) -> houyicoder_async::PFut<
        '_,
        Result<houyicoder_context::ExecResult, houyicoder_context::SandboxError>,
    > {
        Box::pin(async move {
            Ok(houyicoder_context::ExecResult {
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
    ) -> houyicoder_async::PFut<'_, Result<Vec<u8>, houyicoder_context::SandboxError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn write_file(
        &self,
        _path: &str,
        _content: Vec<u8>,
    ) -> houyicoder_async::PFut<'_, Result<(), houyicoder_context::SandboxError>> {
        Box::pin(async move { Ok(()) })
    }
    fn list_dir(
        &self,
        _path: &str,
    ) -> houyicoder_async::PFut<
        '_,
        Result<Vec<houyicoder_context::DirEntry>, houyicoder_context::SandboxError>,
    > {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn path_exists(
        &self,
        _path: &str,
    ) -> houyicoder_async::PFut<'_, Result<bool, houyicoder_context::SandboxError>> {
        Box::pin(async move { Ok(false) })
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(self.root.clone())
    }
}

#[test]
fn test_bash_tool_snapshots_destructive() {
    let tmp = temp_dir().join(format!("bash-snap-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let store = Arc::new(SnapshotStore::new(&tmp).expect("store"));
    let stack = Arc::new(Mutex::new(UndoStack::new()));
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
    remove_dir_all(&tmp).ok();
}

#[test]
fn test_workspace_size_sums_files() {
    let tmp = temp_dir().join(format!("ws-size-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("a.txt"), b"hello").expect("write");
    write(tmp.join("b.txt"), b"world!!").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    let size = store.workspace_size();
    assert!(
        size >= 12,
        "sums file sizes (at least the two files' bytes)"
    );
    remove_dir_all(&tmp).ok();
}

#[test]
fn test_check_reflink_policy_passes() {
    // On APFS (the test runner's FS), reflink should be available, so the
    // policy passes regardless of workspace size.
    let tmp = temp_dir().join(format!("reflink-ok-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let store = SnapshotStore::new(&tmp).expect("store");
    assert!(
        store.check_reflink_policy().is_ok(),
        "policy passes when reflink is available"
    );
    remove_dir_all(&tmp).ok();
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
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert!(err.to_string().contains("degrade to Ask"));
}

#[test]
fn test_undo_stack_push_safe() {
    // Concurrent pushers under the shared Arc<Mutex<UndoStack>> must not
    // panic or corrupt the stack. Deterministic invariant: after all
    // threads join, len == total pushes (no lost entries).
    let stack = Arc::new(Mutex::new(UndoStack::new()));
    let n_threads = 8;
    let per_thread = 500;
    let mut handles = vec![];
    for t in 0..n_threads {
        let s = stack.clone();
        handles.push(thread::spawn(move || {
            for i in 0..per_thread {
                s.lock().unwrap().push(UndoEntry::BeforeImage {
                    path: PathBuf::from(format!("t{t}-{i}")),
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
    let stack = Arc::new(Mutex::new(UndoStack::new()));
    let total = 2000;
    for i in 0..total {
        stack.lock().unwrap().push(UndoEntry::BeforeImage {
            path: PathBuf::from(format!("f{i}")),
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
    let tmp = temp_dir().join(format!("audit-sink-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).unwrap();
    write(tmp.join("f.txt"), "x").unwrap();
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
    remove_dir_all(&tmp).ok();
}

#[test]
fn test_prune_removes_old_snapshots() {
    let tmp = temp_dir().join(format!("prune-old-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    store.snapshot(&[]).expect("snap1");
    store.snapshot(&[]).expect("snap2");
    // Prune with size cap 0 (remove everything) + no protected entries.
    let removed = store.prune(604800, 0, &[]);
    assert!(removed >= 1, "size cap 0 removes at least one snapshot");
    remove_dir_all(&tmp).ok();
}

#[test]
fn test_prune_skips_protected_snapshots() {
    let tmp = temp_dir().join(format!("prune-prot-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    let entry = store.snapshot(&[]).expect("snap");
    let protected = match &entry {
        UndoEntry::CoWSnapshot { store_path, .. } => vec![store_path.clone()],
        _ => vec![],
    };
    // Prune with size cap 0 but the snapshot is protected → 0 removed.
    let removed = store.prune(604800, 0, &protected);
    assert_eq!(removed, 0, "protected snapshot not pruned");
    remove_dir_all(&tmp).ok();
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
    let tmp = temp_dir().join(format!(
        "snap-id-{}-{}",
        id(),
        AtomicU64::new(0).fetch_add(1, Ordering::Relaxed)
    ));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
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
    remove_dir_all(&tmp).ok();
}

/// A re-link hands back the newest snapshot last, so the first pop restores the
/// most recent destructive operation. Reading the ids in reverse put the oldest
/// snapshot on top, which restored the wrong state.
#[test]
fn test_relink_orders_newest_last() {
    let tmp = temp_dir().join(format!("snap-relink-order-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    store.snapshot(&[]).expect("snap-0");
    store.snapshot(&[]).expect("snap-1");
    store.snapshot(&[]).expect("snap-2");
    let resumed = SnapshotStore::new(&tmp).expect("resumed");
    let entries = resumed.relink_undo_entries();
    let ids: Vec<String> = entries
        .iter()
        .map(|e| match e {
            UndoEntry::CoWSnapshot { store_path, .. } => store_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string(),
            _ => String::new(),
        })
        .collect();
    assert_eq!(
        ids,
        vec!["snap-1", "snap-2", "snap-3"],
        "the newest snapshot must be last so pop restores it first"
    );
    remove_dir_all(&tmp).ok();
}

/// A snapshot past the retention TTL is not re-linked: it is left for prune.
/// Re-linking it would mark it protected and prune could never reclaim it.
#[test]
fn test_relink_skips_expired_snapshot() {
    let tmp = temp_dir().join(format!("snap-relink-ttl-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    let fresh = store.snapshot(&[]).expect("fresh");
    let UndoEntry::CoWSnapshot {
        store_path: stale, ..
    } = store.snapshot(&[]).expect("stale")
    else {
        panic!("expected CoWSnapshot");
    };
    let aged = SystemTime::now() - std::time::Duration::from_secs(DEFAULT_SNAPSHOT_TTL_SECS + 60);
    // A directory handle needs backup semantics on Windows; opening it with
    // File::open is refused there, and set_modified maps to SetFileTime, which
    // needs write access. Asking for write plus that flag covers both.
    let mut opts = OpenOptions::new();
    #[cfg(unix)]
    opts.read(true);
    #[cfg(windows)]
    {
        // FILE_FLAG_BACKUP_SEMANTICS, the flag that permits a directory
        // handle. Spelled as its literal value because the constant lives in
        // a windows binding crate this workspace does not depend on.
        const BACKUP_SEMANTICS: u32 = 0x0200_0000;
        opts.write(true).custom_flags(BACKUP_SEMANTICS);
    }
    let handle = opts.open(&stale).expect("open snapshot dir");
    handle.set_modified(aged).expect("age the snapshot");
    let resumed = SnapshotStore::new(&tmp).expect("resumed");
    let entries = resumed.relink_undo_entries();
    let paths: Vec<&PathBuf> = entries
        .iter()
        .map(|e| match e {
            UndoEntry::CoWSnapshot { store_path, .. } => store_path,
            _ => unreachable!(),
        })
        .collect();
    let UndoEntry::CoWSnapshot {
        store_path: fresh_path,
        ..
    } = fresh
    else {
        panic!("expected CoWSnapshot");
    };
    assert_eq!(
        paths,
        vec![&fresh_path],
        "the expired snapshot must be left for prune, not re-linked"
    );
    remove_dir_all(&tmp).ok();
}

/// A store that moves out of the workspace takes its history with it, and a
/// store opened where a previous one lived starts its counter past those ids.
#[test]
fn test_store_move_carries_snapshots() {
    let tmp = temp_dir().join(format!("snap-move-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let legacy = SnapshotStore::new(&tmp).expect("legacy store");
    legacy.snapshot(&[]).expect("snap in the old location");
    drop(legacy);
    let new_root = temp_dir().join(format!("snap-move-out-{}", id()));
    remove_dir_all(&new_root).ok();
    let moved = SnapshotStore::new_at(&tmp, &new_root).expect("moved store");
    let entries = moved.relink_undo_entries();
    assert_eq!(
        entries.len(),
        1,
        "the old store's snapshot must move with the store: {entries:?}"
    );
    let next = moved.snapshot(&[]).expect("snapshot after the move");
    let UndoEntry::CoWSnapshot { store_path, .. } = next else {
        panic!("expected CoWSnapshot");
    };
    assert_eq!(
        store_path.file_name().and_then(|n| n.to_str()),
        Some("snap-2"),
        "the counter continues past the moved snapshot"
    );
    remove_dir_all(&tmp).ok();
    remove_dir_all(&new_root).ok();
}

/// A re-link keeps only the newest MAX_UNDO_DEPTH snapshots. Keeping more
/// would mark every one of them protected, which is what stopped prune from
/// reclaiming anything.
#[test]
fn test_relink_respects_cap() {
    let tmp = temp_dir().join(format!("snap-relink-cap-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    for _ in 0..MAX_UNDO_DEPTH + 3 {
        store.snapshot(&[]).expect("snapshot");
    }
    let resumed = SnapshotStore::new(&tmp).expect("resumed");
    let entries = resumed.relink_undo_entries();
    assert_eq!(
        entries.len(),
        MAX_UNDO_DEPTH,
        "the stack stops at the undo depth"
    );
    let last = match entries.last() {
        Some(UndoEntry::CoWSnapshot { store_path, .. }) => store_path.clone(),
        _ => unreachable!(),
    };
    let highest = scan_max_snapshot_id(&tmp.join(".houyicoder").join("snapshots"));
    assert_eq!(
        last.file_name().and_then(|n| n.to_str()),
        Some(format!("snap-{highest}").as_str()),
        "the kept window is the newest one"
    );
    remove_dir_all(&tmp).ok();
}

/// A store move leaves a snapshot that already exists at the destination
/// alone, and leaves the previous directory in place so the files are still
/// on disk rather than deleted on the floor.
#[test]
fn test_store_move_keeps_existing() {
    let tmp = temp_dir().join(format!("snap-move-collide-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let legacy = SnapshotStore::new(&tmp).expect("legacy");
    legacy.snapshot(&[]).expect("snap in the old location");
    drop(legacy);
    let new_root = temp_dir().join(format!("snap-move-collide-out-{}", id()));
    remove_dir_all(&new_root).ok();
    create_dir_all(new_root.join("snap-1")).expect("pre-create the destination id");
    let moved = SnapshotStore::new_at(&tmp, &new_root).expect("moved store");
    let entries = moved.relink_undo_entries();
    assert_eq!(
        entries.len(),
        1,
        "only the destination's own snapshot re-links: {entries:?}"
    );
    let UndoEntry::CoWSnapshot { store_path, .. } = &entries[0] else {
        panic!("expected CoWSnapshot");
    };
    assert_eq!(
        store_path,
        &new_root.join("snap-1"),
        "the re-linked entry is the destination's, not the collided one"
    );
    assert!(
        tmp.join(".houyicoder")
            .join("snapshots")
            .join("snap-1")
            .is_dir(),
        "the collided snapshot is left on disk, not deleted"
    );
    remove_dir_all(&tmp).ok();
    remove_dir_all(&new_root).ok();
}
