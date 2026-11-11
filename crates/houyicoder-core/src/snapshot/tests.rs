//! Tests for snapshot + undo: the LIFO undo stack, reflink policy,
//! workspace size caps, and restore round-trip.

mod recovery;

use super::lease::LeaseProbe;
use super::lease::claim;
use super::lease::claim_exclusive;
use super::lease::mark_legacy_sentinel;
use super::migrate::migrate_store;
use super::prune::RemovedSnapshot;
use super::prune::cleanup_sidecars;
use super::staging::complete_marker;
use super::*;
use crate::agent::BashTool;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::tool::{Tool, ToolCtx};
use std::env::temp_dir;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::read_dir;
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
use std::time::Duration;
use std::time::SystemTime;
#[cfg(unix)]
use std::{fs::File, fs::Permissions, fs::set_permissions, os::unix::fs::PermissionsExt};

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
/// pointing at them, so a restore still works across a restart. The crash-
/// after-destructive-op case (the gap this closes) is the intersection
/// this test exercises.
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
        lease,
        ..
    } = entry
    else {
        panic!("expected CoWSnapshot");
    };
    drop(lease);
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
        restored_path, &entry_path,
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
        store_path: valid,
        lease,
        ..
    } = entry
    else {
        panic!("expected CoWSnapshot");
    };
    drop(lease);
    let store_dir = tmp.join(".houyicoder").join("snapshots");
    // A junk file in the store dir (skip: not a directory).
    write(store_dir.join("junk-file"), "x").unwrap();
    // A non-snap-N named directory (skip: name does not match snap-<n>).
    create_dir_all(store_dir.join("other-dir")).unwrap();
    // A snap- prefix without a number (skip: no id to re-link).
    create_dir_all(store_dir.join("snap-abc")).unwrap();
    // A snapshot of additional writable roots (skip: the root mapping is not
    // on disk, so restore could not remap the subtree).
    create_dir_all(store_dir.join("snap-98").join("__extra")).unwrap();
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

/// A snapshot re-linked from a version without the complete marker is
/// reported as unverified; a published one is described plainly.
#[test]
fn test_undo_entry_description() {
    let tmp = temp_dir().join(format!("snap-description-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(tmp.join("snap-0")).expect("snapshot dir");
    let cow = UndoEntry::CoWSnapshot {
        store_path: tmp.join("snap-0"),
        extra_roots: vec![],
        lease: SnapshotLease::test_lease(),
    };
    assert!(cow.description().contains("restored"));
    assert!(
        cow.description().contains("unverified"),
        "a snapshot without the complete marker reports itself unverified"
    );
    write(complete_marker(&tmp, "snap-0"), b"").expect("write the complete marker");
    assert!(
        !cow.description().contains("unverified"),
        "a published snapshot is described plainly"
    );
    let img = UndoEntry::BeforeImage {
        path: PathBuf::from("/tmp/f.txt"),
        before: Some(b"x".to_vec()),
    };
    assert!(img.description().contains("restored"));
    remove_dir_all(&tmp).ok();
}

/// Snapshot entries hold an open lease file each, so the stack keeps at most
/// the undo depth of them: the oldest is evicted, and a before-image entry is
/// not counted against the depth.
#[test]
fn test_push_keeps_snapshot_depth() {
    let mut stack = UndoStack::new();
    stack.push(UndoEntry::BeforeImage {
        path: PathBuf::from("first.txt"),
        before: None,
    });
    for index in 0..MAX_UNDO_DEPTH {
        stack.push(UndoEntry::CoWSnapshot {
            store_path: PathBuf::from(format!("snap-{index}")),
            extra_roots: vec![],
            lease: SnapshotLease::test_lease(),
        });
    }
    let depth = stack.len();
    stack.push(UndoEntry::CoWSnapshot {
        store_path: PathBuf::from("snap-new"),
        extra_roots: vec![],
        lease: SnapshotLease::test_lease(),
    });
    assert_eq!(
        stack.len(),
        depth,
        "the oldest snapshot entry is evicted instead of growing the stack"
    );
    let paths = stack.snapshot_paths();
    assert_eq!(paths.len(), MAX_UNDO_DEPTH);
    assert!(
        !paths.contains(&PathBuf::from("snap-0")),
        "the oldest snapshot entry is gone"
    );
    assert!(paths.contains(&PathBuf::from("snap-new")));
    let mut kept_image = false;
    while let Some(entry) = stack.pop() {
        if matches!(entry, UndoEntry::BeforeImage { .. }) {
            kept_image = true;
        }
    }
    assert!(
        kept_image,
        "a before-image entry is not counted against the depth"
    );
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
    // The live entry holds a lease, so only the entry already dropped is
    // prunable under a zero size cap.
    store.prune(604800, 0, &[]);
    let ev = events.lock().unwrap();
    assert_eq!(
        ev.iter().filter(|s| s.as_str() == "created").count(),
        2,
        "two snapshots created: {ev:?}"
    );
    assert!(
        ev.iter().any(|s| s.as_str() == "pruned:1"),
        "one unheld snapshot pruned: {ev:?}"
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
        lease: SnapshotLease::test_lease(),
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
        store_path: fresh_path,
        lease: fresh_lease,
        ..
    } = fresh
    else {
        panic!("expected CoWSnapshot");
    };
    drop(fresh_lease);
    let UndoEntry::CoWSnapshot {
        store_path: stale,
        lease: stale_lease,
        ..
    } = store.snapshot(&[]).expect("stale")
    else {
        panic!("expected CoWSnapshot");
    };
    drop(stale_lease);
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
    assert!(
        new_root.join(".snap-1.complete").is_file(),
        "the moved snapshot keeps its complete marker"
    );
    assert!(
        !tmp.join(".houyicoder")
            .join("snapshots")
            .join(".snap-1.complete")
            .exists(),
        "the marker moves with the snapshot instead of staying behind"
    );
    let left = tmp.join(".houyicoder").join("snapshots");
    assert!(
        !left.join("snap-1").exists(),
        "the moved snapshot directory is gone from the previous store"
    );
    // The previous store's namespace lock stays in place: unlinking it would
    // let a waiter on the old file and a fresh opener both hold it.
    assert!(
        left.join(".snapshots.lock").is_file(),
        "the previous store's namespace lock is left in place"
    );
    assert!(
        !left.join(".snap-1.lease").exists() && !left.join(".snap-1.complete").exists(),
        "the moved sidecars are gone from the previous store"
    );
    assert!(
        !entries[0].description().contains("unverified"),
        "a moved snapshot is still reported as verified: {}",
        entries[0].description()
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
/// on disk rather than deleted on the floor. The collided destination keeps
/// its own sidecar state: the move never adopts a marker onto a directory
/// this run did not move, because that would call an unproven tree verified.
#[test]
fn test_store_move_keeps_existing() {
    let tmp = temp_dir().join(format!("snap-move-collide-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let legacy = SnapshotStore::new(&tmp).expect("legacy");
    legacy.snapshot(&[]).expect("snap in the old location");
    drop(legacy);
    let legacy_root = tmp.join(".houyicoder").join("snapshots");
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
        legacy_root.join("snap-1").is_dir(),
        "the collided snapshot is left on disk, not deleted"
    );
    assert!(
        !new_root.join(".snap-1.complete").exists(),
        "the destination tree is not marked complete by a marker it did not earn"
    );
    assert!(
        legacy_root.join(".snap-1.complete").is_file(),
        "the left-behind snapshot keeps its own marker"
    );
    assert!(
        legacy_root.join(".snap-1.lease").is_file(),
        "the left-behind snapshot keeps its own sentinel"
    );
    remove_dir_all(&tmp).ok();
    remove_dir_all(&new_root).ok();
}

/// Moving the previous location's snapshots needs the destination's
/// namespace lock. When that lock cannot be opened the store still opens and
/// leaves the old snapshots where they are, so a later start can move them.
#[test]
fn test_store_move_needs_lock() {
    let tmp = temp_dir().join(format!("snap-move-lock-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let legacy = SnapshotStore::new(&tmp).expect("legacy store");
    legacy.snapshot(&[]).expect("snap in the old location");
    drop(legacy);
    let new_root = temp_dir().join(format!("snap-move-lock-out-{}", id()));
    remove_dir_all(&new_root).ok();
    create_dir_all(new_root.join(".snapshots.lock")).expect("unopenable lock");
    let moved = SnapshotStore::new_at(&tmp, &new_root).expect("moved store");
    assert!(
        tmp.join(".houyicoder")
            .join("snapshots")
            .join("snap-1")
            .is_dir(),
        "the old snapshot stays where it was"
    );
    assert!(
        !new_root.join("snap-1").exists(),
        "nothing moved without the lock"
    );
    assert!(
        moved.relink_undo_entries().is_empty(),
        "the new root holds no snapshot"
    );
    remove_dir_all(&tmp).ok();
    remove_dir_all(&new_root).ok();
}

/// A previous store that another process is still using is left untouched:
/// the move needs its namespace lock, and a later start picks the snapshots
/// up once the lock is free.
#[test]
fn test_move_skips_active_store() {
    let tmp = temp_dir().join(format!("snap-move-active-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let legacy = SnapshotStore::new(&tmp).expect("legacy store");
    legacy.snapshot(&[]).expect("snap in the old location");
    drop(legacy);
    let legacy_root = tmp.join(".houyicoder").join("snapshots");
    let held = claim_exclusive(&legacy_root.join(".snapshots.lock")).expect("hold the store");
    let new_root = temp_dir().join(format!("snap-move-active-out-{}", id()));
    remove_dir_all(&new_root).ok();
    SnapshotStore::new_at(&tmp, &new_root).expect("moved store");
    assert!(
        legacy_root.join("snap-1").is_dir(),
        "a store in use keeps its snapshots"
    );
    assert!(
        !new_root.join("snap-1").exists(),
        "nothing moves while the previous store is in use"
    );
    drop(held);
    let moved = SnapshotStore::new_at(&tmp, &new_root).expect("moved again");
    assert_eq!(
        moved.relink_undo_entries().len(),
        1,
        "a later start completes the move"
    );
    remove_dir_all(&tmp).ok();
    remove_dir_all(&new_root).ok();
}

/// A snapshot another process holds stays where it is, so a session that
/// moved its store never takes a tree a live session is still using.
#[test]
fn test_move_skips_held_snapshot() {
    let tmp = temp_dir().join(format!("snap-move-held-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let legacy = SnapshotStore::new(&tmp).expect("legacy store");
    legacy.snapshot(&[]).expect("snap in the old location");
    drop(legacy);
    let legacy_root = tmp.join(".houyicoder").join("snapshots");
    let held = claim_exclusive(&legacy_root.join(".snap-1.lease")).expect("hold the snapshot");
    let new_root = temp_dir().join(format!("snap-move-held-out-{}", id()));
    remove_dir_all(&new_root).ok();
    let moved = SnapshotStore::new_at(&tmp, &new_root).expect("moved store");
    assert!(
        legacy_root.join("snap-1").is_dir(),
        "a snapshot a live session holds stays where it is"
    );
    assert!(
        !new_root.join("snap-1").exists(),
        "the held snapshot is not moved"
    );
    assert!(
        !new_root.join(".snap-1.complete").exists(),
        "no marker lands at the destination without its snapshot"
    );
    assert!(
        moved.relink_undo_entries().is_empty(),
        "the new root holds no snapshot"
    );
    drop(held);
    remove_dir_all(&tmp).ok();
    remove_dir_all(&new_root).ok();
}

/// A destination sentinel held by another process means that process still
/// owns the name, for example a prune deleting it. The move skips the
/// snapshot instead of replacing the sentinel, and a later start completes
/// the move once the lease is free.
#[test]
fn test_move_skips_held_target() {
    let tmp = temp_dir().join(format!("snap-move-held-target-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let legacy = SnapshotStore::new(&tmp).expect("legacy store");
    legacy.snapshot(&[]).expect("snap in the old location");
    drop(legacy);
    let legacy_root = tmp.join(".houyicoder").join("snapshots");
    let new_root = temp_dir().join(format!("snap-move-held-target-out-{}", id()));
    remove_dir_all(&new_root).ok();
    create_dir_all(&new_root).expect("destination root");
    let held = claim_exclusive(&new_root.join(".snap-1.lease")).expect("hold the destination name");
    SnapshotStore::new_at(&tmp, &new_root).expect("moved store");
    assert!(
        legacy_root.join("snap-1").is_dir(),
        "the snapshot stays where it is while its destination name is held"
    );
    assert!(
        !new_root.join("snap-1").exists(),
        "nothing moves onto a name another process holds"
    );
    assert!(
        new_root.join(".snap-1.lease").is_file(),
        "the held sentinel is never replaced by the move"
    );
    assert!(
        !new_root.join(".snap-1.complete").exists(),
        "no marker lands without its snapshot"
    );
    drop(held);
    let moved = SnapshotStore::new_at(&tmp, &new_root).expect("moved again");
    assert_eq!(
        moved.relink_undo_entries().len(),
        1,
        "a later start moves the snapshot once the destination is free"
    );
    remove_dir_all(&tmp).ok();
    remove_dir_all(&new_root).ok();
}

/// A snapshot from before markers and leases moves with a destination
/// sentinel tagged as pre-marker data, so the resumed session re-links it and
/// reports it as unverified instead of refusing its own moved history.
#[test]
fn test_move_tags_unmarked_snapshot() {
    let tmp = temp_dir().join(format!("snap-move-legacy-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let legacy_root = tmp.join(".houyicoder").join("snapshots");
    let old = legacy_root.join("snap-3");
    create_dir_all(&old).expect("legacy snapshot");
    write(old.join("hello.txt"), "world").expect("copy");
    let new_root = temp_dir().join(format!("snap-move-legacy-out-{}", id()));
    remove_dir_all(&new_root).ok();
    let moved = SnapshotStore::new_at(&tmp, &new_root).expect("moved store");
    let entries = moved.relink_undo_entries();
    assert_eq!(entries.len(), 1, "the moved legacy snapshot re-links");
    assert!(
        entries[0].description().contains("unverified"),
        "a moved pre-marker snapshot is reported as unverified: {}",
        entries[0].description()
    );
    remove_dir_all(&tmp).ok();
    remove_dir_all(&new_root).ok();
}

/// A sidecar that cannot move stops the whole move for that snapshot: the
/// directory never travels without the marker that makes it verified. A later
/// start finishes the move once the obstruction is gone.
#[test]
fn test_move_aborts_on_sidecar() {
    let tmp = temp_dir().join(format!("snap-move-sidecar-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let legacy = SnapshotStore::new(&tmp).expect("legacy store");
    legacy.snapshot(&[]).expect("snap in the old location");
    drop(legacy);
    let legacy_root = tmp.join(".houyicoder").join("snapshots");
    let new_root = temp_dir().join(format!("snap-move-sidecar-out-{}", id()));
    remove_dir_all(&new_root).ok();
    create_dir_all(new_root.join(".snap-1.complete")).expect("block the marker move");
    SnapshotStore::new_at(&tmp, &new_root).expect("moved store");
    assert!(
        legacy_root.join("snap-1").is_dir(),
        "the snapshot stays where it is when a sidecar cannot move"
    );
    assert!(
        !new_root.join("snap-1").exists(),
        "the directory never moves without its marker"
    );
    assert!(
        legacy_root.join(".snap-1.complete").is_file(),
        "the marker stays with the snapshot it describes"
    );
    remove_dir_all(new_root.join(".snap-1.complete")).expect("clear the obstruction");
    let moved = SnapshotStore::new_at(&tmp, &new_root).expect("moved again");
    assert_eq!(
        moved.relink_undo_entries().len(),
        1,
        "a later start finishes the move"
    );
    remove_dir_all(&tmp).ok();
    remove_dir_all(&new_root).ok();
}

/// A sentinel beside a directory that has no complete marker means a publish
/// never finished. Re-link refuses that tree, and accepts it only once its
/// sentinel records it as pre-marker data, so a half-written tree never
/// reaches the undo stack.
#[test]
fn test_relink_refuses_untagged() {
    let tmp = temp_dir().join(format!("snap-relink-untagged-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let snaps = tmp.join(".houyicoder").join("snapshots");
    let snapshot = snaps.join("snap-2");
    create_dir_all(&snapshot).expect("snapshot dir");
    write(snapshot.join("hello.txt"), "world").expect("copy");
    let sentinel = snaps.join(".snap-2.lease");
    write(&sentinel, "").expect("empty sentinel");
    let store = SnapshotStore::new(&tmp).expect("store");
    assert!(
        store.relink_undo_entries().is_empty(),
        "a lease without a marker is refused"
    );
    assert!(
        snapshot.is_dir(),
        "the refused snapshot stays on disk for an operator"
    );
    mark_legacy_sentinel(&sentinel).expect("tag the sentinel");
    let entries = store.relink_undo_entries();
    assert_eq!(entries.len(), 1, "a tagged snapshot re-links");
    assert!(
        entries[0].description().contains("unverified"),
        "a pre-marker snapshot is reported as unverified: {}",
        entries[0].description()
    );
    remove_dir_all(&tmp).ok();
}

/// Prune removes a snapshot's sidecars only while its directory is still
/// gone. A directory published again under the same name keeps them, because
/// they now describe the newer tree.
#[test]
fn test_cleanup_keeps_reappeared_sidecars() {
    let tmp = temp_dir().join(format!("snap-cleanup-returned-{}", id()));
    remove_dir_all(&tmp).ok();
    let snaps = tmp.join(".houyicoder").join("snapshots");
    let snapshot = snaps.join("snap-4");
    create_dir_all(&snapshot).expect("snapshot dir");
    write(snapshot.join("hello.txt"), "world").expect("copy");
    let sentinel = snaps.join(".snap-4.lease");
    write(&sentinel, "").expect("sentinel");
    let marker = snaps.join(".snap-4.complete");
    write(&marker, "").expect("marker");
    let lease = claim_exclusive(&sentinel).expect("claim the sentinel");
    let kept = cleanup_sidecars(
        &snaps,
        vec![RemovedSnapshot {
            directory: snapshot.clone(),
            sentinel: sentinel.clone(),
            marker: marker.clone(),
            lease,
        }],
    );
    assert_eq!(kept, 1, "a directory that came back keeps its sidecars");
    assert!(sentinel.is_file(), "the sentinel survives");
    assert!(marker.is_file(), "the marker survives");
    remove_dir_all(&tmp).ok();
}

/// A store root that cannot be created stops the move before any snapshot
/// is renamed out of the previous location.
#[test]
fn test_migrate_needs_store_dir() {
    let tmp = temp_dir().join(format!("snap-migrate-nodir-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(tmp.join("legacy").join("snap-1")).expect("legacy snapshot");
    write(tmp.join("legacy").join("snap-1").join("hello.txt"), "world").expect("copy");
    write(
        tmp.join("legacy").join("snap-1").join(".snap-1.complete"),
        "",
    )
    .expect("marker");
    let blocker = tmp.join("blocker");
    write(&blocker, "x").expect("write a file where the store parent belongs");
    let StoreProbe::Claimed(lease) = try_store_lock(&tmp.join("legacy")) else {
        panic!("the legacy lock is free");
    };
    let destination = tmp.join("destination");
    let StoreProbe::Claimed(destination_lock) = try_store_lock(&destination) else {
        panic!("the destination lock is free");
    };
    migrate_store(
        &tmp.join("legacy"),
        &blocker.join("store"),
        lease,
        destination_lock,
    );
    assert!(
        tmp.join("legacy").join("snap-1").is_dir(),
        "nothing moves without a store root"
    );
    assert!(
        !blocker.join("store").exists(),
        "a store root that cannot be created is not created"
    );
    remove_dir_all(&tmp).ok();
}

/// A previous store that cannot be listed keeps its snapshots, so a
/// permission problem never reads as an empty store.
#[cfg(unix)]
#[test]
fn test_migrate_skips_unreadable() {
    let tmp = temp_dir().join(format!("snap-migrate-unreadable-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(tmp.join("legacy").join("snap-1")).expect("legacy snapshot");
    let legacy = tmp.join("legacy");
    let store_root = tmp.join("store");
    let StoreProbe::Claimed(lease) = try_store_lock(&legacy) else {
        panic!("the legacy lock is free");
    };
    let StoreProbe::Claimed(destination_lock) = try_store_lock(&tmp.join("destination")) else {
        panic!("the destination lock is free");
    };
    set_permissions(&legacy, Permissions::from_mode(0o000)).expect("close the legacy store");
    // A privileged runner ignores the mode, so the branch under test is not
    // reachable there and the test ends without asserting.
    if read_dir(&legacy).is_ok() {
        set_permissions(&legacy, Permissions::from_mode(0o755)).ok();
        remove_dir_all(&tmp).ok();
        return;
    }
    migrate_store(&legacy, &store_root, lease, destination_lock);
    set_permissions(&legacy, Permissions::from_mode(0o755)).expect("restore the legacy store");
    assert!(
        legacy.join("snap-1").is_dir(),
        "an unreadable store keeps its snapshots"
    );
    assert!(
        !store_root.join("snap-1").exists(),
        "nothing moves out of an unreadable store"
    );
    remove_dir_all(&tmp).ok();
}

/// A previous store whose lock cannot be opened is not read at all, so the
/// move is skipped and a later start tries again.
#[cfg(unix)]
#[test]
fn test_move_skips_unreadable_store() {
    let tmp = temp_dir().join(format!("snap-move-unreadable-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let legacy = SnapshotStore::new(&tmp).expect("legacy store");
    legacy.snapshot(&[]).expect("snap in the old location");
    drop(legacy);
    let legacy_root = tmp.join(".houyicoder").join("snapshots");
    set_permissions(&legacy_root, Permissions::from_mode(0o000)).expect("close the previous store");
    let new_root = temp_dir().join(format!("snap-move-unreadable-out-{}", id()));
    remove_dir_all(&new_root).ok();
    let moved = SnapshotStore::new_at(&tmp, &new_root).expect("store");
    set_permissions(&legacy_root, Permissions::from_mode(0o755)).expect("restore the store");
    assert!(
        legacy_root.join("snap-1").is_dir(),
        "a store that cannot be read keeps its snapshots"
    );
    assert!(
        !new_root.join("snap-1").exists(),
        "nothing moves out of an unreadable store"
    );
    assert!(
        moved.relink_undo_entries().is_empty(),
        "the new root holds no snapshot"
    );
    remove_dir_all(&tmp).ok();
    remove_dir_all(&new_root).ok();
}

/// A snapshot held by an undo entry belongs to a live session. Prune runs
/// in another process and sees an empty protected list, so only the lease
/// keeps the directory: a zero size cap must still leave it alone.
#[test]
fn test_prune_keeps_leased_snapshot() {
    let tmp = temp_dir().join(format!("snap-prune-lease-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    let entry = store.snapshot(&[]).expect("snapshot");
    let UndoEntry::CoWSnapshot { store_path, .. } = &entry else {
        panic!("expected CoWSnapshot");
    };
    let store_path = store_path.clone();
    let removed = store.prune(604800, 0, &[]);
    assert_eq!(removed, 0, "a held snapshot is not pruned");
    assert!(store_path.is_dir(), "the held snapshot stays on disk");
    drop(entry);
    let removed = store.prune(604800, 0, &[]);
    assert_eq!(removed, 1, "once released the snapshot is prunable");
    assert!(!store_path.exists(), "the released snapshot is removed");
    remove_dir_all(&tmp).ok();
}

/// A staging tree is private to the process building it. Prune must not
/// count it as a snapshot, and a zero size cap must leave its staged bytes
/// for the builder that owns them.
#[test]
fn test_prune_skips_staging_build() {
    let tmp = temp_dir().join(format!("snap-prune-staging-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let store = SnapshotStore::new(&tmp).expect("store");
    let staging = tmp
        .join(".houyicoder")
        .join("snapshots")
        .join(format!(".staging-{}-0", id()));
    create_dir_all(&staging).expect("staging dir");
    write(staging.join("partial.txt"), "half").expect("staged file");
    let removed = store.prune(604800, 0, &[]);
    assert_eq!(removed, 0, "a staging tree is not a prunable snapshot");
    assert!(staging.is_dir(), "the staging tree is left for its builder");
    assert!(
        staging.join("partial.txt").is_file(),
        "staged bytes stay for the builder"
    );
    remove_dir_all(&tmp).ok();
}

/// A build that died leaves its staging tree behind. Once the safe period
/// has passed and the lease is free, prune reclaims the tree and its
/// sentinel, and does not count either as a removed snapshot.
#[test]
fn test_prune_reclaims_stale_staging() {
    let tmp = temp_dir().join(format!("snap-prune-orphan-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let store = SnapshotStore::new(&tmp).expect("store");
    let snapshots = tmp.join(".houyicoder").join("snapshots");
    let name = format!(".staging-{}-0", id());
    let staging = snapshots.join(&name);
    create_dir_all(&staging).expect("staging dir");
    write(staging.join("half.txt"), "x").expect("staged file");
    let sentinel = snapshots.join(format!("{name}.lease"));
    write(&sentinel, b"").expect("lease file");
    let aged = SystemTime::now() - Duration::from_secs(7200);
    let handle = OpenOptions::new()
        .write(true)
        .open(&sentinel)
        .expect("open the lease");
    handle.set_modified(aged).expect("age the lease");
    let removed = store.prune(604800, u64::MAX, &[]);
    assert_eq!(removed, 0, "a staging tree is not a snapshot");
    assert!(!staging.exists(), "the stale staging tree is reclaimed");
    assert!(!sentinel.exists(), "its lease sentinel is reclaimed");
    remove_dir_all(&tmp).ok();
}

/// Move a file's modification time past the safe period, so the cleanup
/// sweep treats whatever it records as abandoned.
fn age_file(path: &Path) {
    let aged = SystemTime::now() - Duration::from_secs(7200);
    OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open to age")
        .set_modified(aged)
        .expect("age the file");
}

/// Assert that no staging tree or staging sentinel remains under the store.
fn assert_no_staging(snaps: &Path) {
    let leftovers: Vec<String> = read_dir(snaps)
        .expect("read the store")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".staging-"))
        .collect();
    assert!(leftovers.is_empty(), "staging files remain: {leftovers:?}");
}

/// A publish that dies between claiming its name and renaming leaves a lease
/// or marker with no snapshot directory. Once the safe period has passed and
/// nothing holds the lease, prune reclaims the sidecar. A fresh sidecar and a
/// plain file that merely shares the staging prefix are left alone.
#[test]
fn test_prune_reclaims_orphans() {
    let tmp = temp_dir().join(format!("snap-prune-sidecar-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let store = SnapshotStore::new(&tmp).expect("store");
    let snaps = tmp.join(".houyicoder").join("snapshots");
    create_dir_all(&snaps).expect("store dir");
    let staging_lease = snaps.join(format!(".staging-{}-9.lease", id()));
    let marker = snaps.join(".snap-9.complete");
    let fresh = snaps.join(".snap-8.complete");
    let plain = snaps.join(format!(".staging-{}-8", id()));
    for path in [&staging_lease, &marker, &plain] {
        write(path, b"").expect("sidecar");
        age_file(path);
    }
    write(&fresh, b"").expect("fresh sidecar");
    let removed = store.prune(604800, u64::MAX, &[]);
    assert_eq!(removed, 0, "a sidecar is not a snapshot");
    assert!(
        !staging_lease.exists(),
        "a staging lease with no tree is reclaimed"
    );
    assert!(!marker.exists(), "a marker with no snapshot is reclaimed");
    assert!(
        !snaps.join(".snap-9.lease").exists(),
        "the claimed lease of an orphan marker is reclaimed with it"
    );
    assert!(fresh.exists(), "a sidecar inside the safe period is kept");
    assert!(plain.exists(), "a plain file is not a staging tree");
    let doubled: Vec<String> = read_dir(&snaps)
        .expect("read the store")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".."))
        .collect();
    assert!(
        doubled.is_empty(),
        "an orphan sidecar leaves no second lease behind: {doubled:?}"
    );
    remove_dir_all(&tmp).ok();
}

/// A snapshot lease belongs to one live process at a time. While another
/// holder owns the lease, re-link leaves the snapshot out of its undo stack;
/// once the lease is free the same snapshot re-links.
#[test]
fn test_lease_blocks_relink() {
    let tmp = temp_dir().join(format!("snap-relink-held-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let snaps = tmp.join(".houyicoder").join("snapshots");
    let snapshot = snaps.join("snap-7");
    create_dir_all(&snapshot).expect("snapshot dir");
    write(snapshot.join("half.txt"), "x").expect("snapshot copy");
    write(snaps.join(".snap-7.complete"), "").expect("complete marker");
    let store = SnapshotStore::new(&tmp).expect("store");
    let held = claim_exclusive(&snaps.join(".snap-7.lease")).expect("hold the lease");
    assert!(
        store.relink_undo_entries().is_empty(),
        "a snapshot another process holds is not re-linked"
    );
    drop(held);
    let entries = store.relink_undo_entries();
    assert_eq!(
        entries.len(),
        1,
        "the released snapshot re-links: {entries:?}"
    );
    remove_dir_all(&tmp).ok();
}

/// A name whose lease another holder keeps busy is skipped, so creating a
/// snapshot never fails because a stale or deleting entry owns one id.
#[test]
fn test_snapshot_skips_held_name() {
    let tmp = temp_dir().join(format!("snap-held-name-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let snaps = tmp.join(".houyicoder").join("snapshots");
    create_dir_all(&snaps).expect("store dir");
    let held = claim_exclusive(&snaps.join(".snap-1.lease")).expect("hold the first id");
    let store = SnapshotStore::new(&tmp).expect("store");
    let entry = store.snapshot(&[]).expect("snapshot");
    let UndoEntry::CoWSnapshot { store_path, .. } = &entry else {
        panic!("expected CoWSnapshot");
    };
    assert_eq!(
        store_path.file_name().and_then(|name| name.to_str()),
        Some("snap-2"),
        "the held name is skipped"
    );
    drop(held);
    remove_dir_all(&tmp).ok();
}

/// A name that already holds a directory is skipped, so a snapshot never
/// publishes over a tree another version left behind.
#[test]
fn test_snapshot_skips_taken_name() {
    let tmp = temp_dir().join(format!("snap-taken-name-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    let snaps = tmp.join(".houyicoder").join("snapshots");
    create_dir_all(snaps.join("snap-1")).expect("existing name");
    let entry = store.snapshot(&[]).expect("snapshot");
    let UndoEntry::CoWSnapshot { store_path, .. } = &entry else {
        panic!("expected CoWSnapshot");
    };
    assert_eq!(
        store_path.file_name().and_then(|name| name.to_str()),
        Some("snap-2"),
        "a name already taken is skipped"
    );
    remove_dir_all(&tmp).ok();
}

/// A name whose lease cannot be read is not taken: the build fails and
/// removes its staging tree, rather than publishing beside a snapshot
/// another process may be deleting.
#[test]
fn test_snapshot_refuses_unreadable_lease() {
    let tmp = temp_dir().join(format!("snap-unreadable-lease-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let snaps = tmp.join(".houyicoder").join("snapshots");
    create_dir_all(snaps.join(".snap-1.lease")).expect("unreadable lease");
    let store = SnapshotStore::new(&tmp).expect("store");
    assert!(
        store.snapshot(&[]).is_err(),
        "an unreadable lease fails the build"
    );
    assert!(!snaps.join("snap-1").exists(), "the name is not taken");
    assert_no_staging(&snaps);
    remove_dir_all(&tmp).ok();
}

/// A publish whose complete marker cannot be written fails without
/// publishing the tree, so no process ever sees a snapshot directory this
/// version cannot prove complete.
#[test]
fn test_snapshot_refuses_unwritable_marker() {
    let tmp = temp_dir().join(format!("snap-unwritable-marker-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let snaps = tmp.join(".houyicoder").join("snapshots");
    create_dir_all(snaps.join(".snap-1.complete")).expect("unwritable marker");
    let store = SnapshotStore::new(&tmp).expect("store");
    assert!(
        store.snapshot(&[]).is_err(),
        "a marker write failure fails the build"
    );
    assert!(
        !snaps.join("snap-1").exists(),
        "nothing is published under the name"
    );
    assert_no_staging(&snaps);
    remove_dir_all(&tmp).ok();
}

/// A legacy directory re-links on every start, so a restarted session keeps
/// the undo entries an earlier start recovered. The first re-link claims a
/// lease and leaves its sentinel behind; that sentinel must not hide the
/// snapshot from the next re-link.
#[test]
fn test_relink_after_restart() {
    let tmp = temp_dir().join(format!("snap-relink-legacy-twice-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let snaps = tmp.join(".houyicoder").join("snapshots");
    let legacy = snaps.join("snap-3");
    create_dir_all(&legacy).expect("legacy dir");
    write(legacy.join("hello.txt"), "world").expect("legacy copy");
    let store = SnapshotStore::new(&tmp).expect("store");
    let first = store.relink_undo_entries();
    assert_eq!(first.len(), 1, "the legacy snapshot re-links");
    drop(first);
    let second = SnapshotStore::new(&tmp)
        .expect("second store")
        .relink_undo_entries();
    assert_eq!(
        second.len(),
        1,
        "a re-linked legacy snapshot still re-links"
    );
    assert!(
        !complete_marker(&snaps, "snap-3").exists(),
        "re-link does not mark a legacy snapshot complete"
    );
    remove_dir_all(&tmp).ok();
}

/// A snapshot written by an earlier version has no complete marker and no
/// lease sidecar. Re-link still accepts it, as the earlier version did, and
/// leaves it unmarked: without a marker the old directory cannot be proven
/// complete, so nothing may record it as verified.
#[test]
fn test_relink_accepts_legacy() {
    let tmp = temp_dir().join(format!("snap-relink-legacy-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let snaps = tmp.join(".houyicoder").join("snapshots");
    let legacy = snaps.join("snap-5");
    create_dir_all(&legacy).expect("legacy dir");
    write(legacy.join("hello.txt"), "world").expect("legacy copy");
    let store = SnapshotStore::new(&tmp).expect("store");
    let entries = store.relink_undo_entries();
    assert_eq!(entries.len(), 1, "a legacy snapshot still re-links");
    assert!(
        !complete_marker(&snaps, "snap-5").exists(),
        "a legacy snapshot is not marked complete by re-link"
    );
    remove_dir_all(&tmp).ok();
}

/// A session that never took a snapshot leaves no store directory behind,
/// including after a re-link on start.
#[test]
fn test_relink_leaves_no_store() {
    let tmp = temp_dir().join(format!("snap-relink-empty-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let store = SnapshotStore::new(&tmp).expect("store");
    assert!(
        store.relink_undo_entries().is_empty(),
        "an unused store holds nothing to re-link"
    );
    assert!(
        !tmp.join(".houyicoder").join("snapshots").exists(),
        "a re-link on a fresh workspace creates no store"
    );
    remove_dir_all(&tmp).ok();
}

/// A store holding more snapshots than the undo depth re-links the newest
/// ones only, oldest first, so a resume acts on the entries it can still use.
#[test]
fn test_relink_keeps_newest() {
    let tmp = temp_dir().join(format!("snap-relink-depth-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let snaps = tmp.join(".houyicoder").join("snapshots");
    for index in 0..(MAX_UNDO_DEPTH + 2) {
        let dir = snaps.join(format!("snap-{index}"));
        create_dir_all(&dir).expect("snapshot dir");
        write(dir.join("hello.txt"), "world").expect("snapshot copy");
    }
    let entries = SnapshotStore::new(&tmp)
        .expect("store")
        .relink_undo_entries();
    assert_eq!(
        entries.len(),
        MAX_UNDO_DEPTH,
        "the stack stops at the undo depth"
    );
    let names: Vec<String> = entries
        .iter()
        .map(|entry| match entry {
            UndoEntry::CoWSnapshot { store_path, .. } => store_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("")
                .to_string(),
            UndoEntry::BeforeImage { .. } => panic!("a relink returns workspace snapshots only"),
        })
        .collect();
    assert_eq!(
        names.first().map(String::as_str),
        Some("snap-2"),
        "the oldest usable snapshot comes first"
    );
    assert_eq!(
        names.last().map(String::as_str),
        Some("snap-11"),
        "the newest snapshot ends the stack"
    );
    remove_dir_all(&tmp).ok();
}

/// An unreadable lease is treated as held, never as free. A snapshot whose
/// claim cannot be checked is left on disk for a later run.
#[cfg(unix)]
#[test]
fn test_prune_skips_unreadable_lease() {
    let tmp = temp_dir().join(format!("snap-prune-unreadable-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    let entry = store.snapshot(&[]).expect("snapshot");
    let UndoEntry::CoWSnapshot { store_path, .. } = &entry else {
        panic!("expected CoWSnapshot");
    };
    let store_path = store_path.clone();
    let name = store_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("snapshot name")
        .to_string();
    drop(entry);
    let lease = store_path.with_file_name(format!(".{name}.lease"));
    set_permissions(&lease, Permissions::from_mode(0o000)).expect("close the lease mode");
    // A privileged runner ignores the mode, so the check has nothing to
    // observe there and the test ends without asserting.
    if OpenOptions::new().write(true).open(&lease).is_ok() {
        set_permissions(&lease, Permissions::from_mode(0o600)).ok();
        remove_dir_all(&tmp).ok();
        return;
    }
    let removed = store.prune(604800, 0, &[]);
    assert_eq!(removed, 0, "a snapshot with an unreadable lease is kept");
    assert!(store_path.is_dir(), "the snapshot stays on disk");
    set_permissions(&lease, Permissions::from_mode(0o600)).ok();
    remove_dir_all(&tmp).ok();
}

/// A workspace file named like the complete marker is the user's content,
/// not store metadata: the snapshot copies it whole and restore returns it.
#[test]
fn test_marker_file_is_content() {
    let tmp = temp_dir().join(format!("snap-complete-file-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join(".complete"), "user bytes").expect("write the workspace file");
    let store = SnapshotStore::new(&tmp).expect("store");
    let entry = store.snapshot(&[]).expect("snapshot");
    let UndoEntry::CoWSnapshot { store_path, .. } = &entry else {
        panic!("expected CoWSnapshot");
    };
    assert_eq!(
        read_to_string(store_path.join(".complete")).expect("read the snapshot copy"),
        "user bytes",
        "the snapshot copies the workspace file whole"
    );
    write(tmp.join(".complete"), "changed").expect("mutate the workspace file");
    store.restore(&entry).expect("restore");
    assert_eq!(
        read_to_string(tmp.join(".complete")).expect("read the restored file"),
        "user bytes",
        "restore returns the workspace file"
    );
    remove_dir_all(&tmp).ok();
}

/// A workspace directory named like the complete marker must not fail the
/// snapshot: the published marker is a file beside the snapshot directory,
/// never an entry inside the copied tree.
#[test]
fn test_marker_dir_is_content() {
    let tmp = temp_dir().join(format!("snap-complete-dir-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(tmp.join(".complete").join("inner")).expect("mkdir");
    write(tmp.join(".complete").join("inner").join("leaf.txt"), "leaf").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    let entry = store
        .snapshot(&[])
        .expect("a workspace directory named like the marker is plain content");
    remove_dir_all(tmp.join(".complete")).expect("mutate the workspace tree");
    store.restore(&entry).expect("restore");
    assert_eq!(
        read_to_string(tmp.join(".complete").join("inner").join("leaf.txt"))
            .expect("read the restored leaf"),
        "leaf",
        "restore returns the nested workspace file"
    );
    remove_dir_all(&tmp).ok();
}

/// A re-linked snapshot belongs to the resumed session. The re-link claims
/// its lease, so prune cannot take a snapshot the undo stack still offers,
/// and dropping the entry makes the snapshot prunable again.
#[test]
fn test_relink_claims_lease() {
    let tmp = temp_dir().join(format!("snap-relink-lease-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    let entry = store.snapshot(&[]).expect("snapshot");
    drop(entry);
    let mut entries = store.relink_undo_entries();
    assert_eq!(entries.len(), 1, "the surviving snapshot re-links");
    let entry = entries.pop().expect("the re-linked entry");
    let UndoEntry::CoWSnapshot { store_path, .. } = &entry else {
        panic!("expected CoWSnapshot");
    };
    assert_eq!(
        store.prune(604800, 0, &[]),
        0,
        "a re-linked snapshot is held by its entry"
    );
    assert!(store_path.is_dir(), "the re-linked snapshot stays on disk");
    drop(entry);
    assert_eq!(
        store.prune(604800, 0, &[]),
        1,
        "dropping the entry releases the snapshot"
    );
    remove_dir_all(&tmp).ok();
}

/// The undo stack keeps a bounded window of snapshot entries, and each entry
/// holds one lease. The entry evicted from the window releases its lease, so
/// the size cap reclaims exactly that snapshot and leaves the live window.
#[test]
fn test_push_releases_evicted() {
    let tmp = temp_dir().join(format!("snap-evict-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    let mut stack = UndoStack::new();
    for _ in 0..=MAX_UNDO_DEPTH {
        let entry = store.snapshot(&[]).expect("snapshot");
        stack.push(entry);
    }
    let evicted = tmp.join(".houyicoder").join("snapshots").join("snap-1");
    let protected = stack.snapshot_paths();
    assert_eq!(
        protected.len(),
        MAX_UNDO_DEPTH,
        "the stack keeps one undo window"
    );
    assert_eq!(
        store.prune(604800, 0, &protected),
        1,
        "only the evicted snapshot is reclaimable"
    );
    assert!(!evicted.exists(), "the evicted snapshot is reclaimed");
    assert_eq!(
        stack.snapshot_paths().len(),
        MAX_UNDO_DEPTH,
        "the live window is unchanged"
    );
    remove_dir_all(&tmp).ok();
}

/// A lease is a kernel lock on the open file handle. A second holder is
/// refused without waiting, and dropping the first frees the lease.
#[test]
fn test_lease_rejects_second_owner() {
    let tmp = temp_dir().join(format!("snap-lease-owner-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let path = tmp.join("lease");
    let lease = match claim(&path) {
        LeaseProbe::Claimed(lease) => lease,
        _ => panic!("the first claim owns the lease"),
    };
    assert!(matches!(claim(&path), LeaseProbe::Busy));
    let error = claim_exclusive(&path).expect_err("a held lease is not handed out");
    assert_eq!(error.kind(), ErrorKind::WouldBlock);
    drop(lease);
    assert!(
        claim_exclusive(&path).is_ok(),
        "the released lease is claimable"
    );
    remove_dir_all(&tmp).ok();
}

/// A lease whose directory cannot be created is unreadable, never free, so
/// the caller keeps the snapshot for a later run.
#[test]
fn test_lease_unreadable_is_kept() {
    let tmp = temp_dir().join(format!("snap-lease-unreadable-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let blocker = tmp.join("blocker");
    write(&blocker, "x").expect("write a file where the directory belongs");
    let path = blocker.join("lease");
    assert!(matches!(claim(&path), LeaseProbe::Unreadable));
    let error = claim_exclusive(&path).expect_err("an unreadable lease is not claimed");
    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    remove_dir_all(&tmp).ok();
}

/// A store directory that does not exist holds nothing to prune.
#[test]
fn test_prune_missing_store() {
    let tmp = temp_dir().join(format!("snap-prune-missing-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let store = SnapshotStore::new_at(&tmp, tmp.join("missing-store")).expect("store");
    assert_eq!(
        store.prune(604800, 0, &[]),
        0,
        "an absent store has nothing to prune"
    );
    remove_dir_all(&tmp).ok();
}

/// A store path that is a file cannot be listed, and prune reports the
/// problem instead of deleting anything.
#[test]
fn test_prune_ignores_non_store() {
    let tmp = temp_dir().join(format!("snap-prune-file-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let root = tmp.join("snapshots-file");
    write(&root, "not a store").expect("write a file where the store belongs");
    let store = SnapshotStore::new_at(&tmp, &root).expect("store");
    assert_eq!(
        store.prune(604800, 0, &[]),
        0,
        "a store path that is a file is left alone"
    );
    assert!(root.is_file(), "the file is untouched");
    remove_dir_all(&tmp).ok();
}

/// A snapshot older than the retention window is removed by expiry even when
/// the size cap would keep everything.
#[cfg(unix)]
#[test]
fn test_prune_removes_expired() {
    let tmp = temp_dir().join(format!("snap-prune-expired-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    let entry = store.snapshot(&[]).expect("snapshot");
    let UndoEntry::CoWSnapshot { store_path, .. } = &entry else {
        panic!("expected CoWSnapshot");
    };
    let store_path = store_path.clone();
    let name = store_path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("snapshot name")
        .to_string();
    drop(entry);
    let directory = File::open(&store_path).expect("open the snapshot directory");
    let aged = SystemTime::now() - Duration::from_secs(604800 + 60);
    directory
        .set_modified(aged)
        .expect("age the snapshot directory");
    assert_eq!(
        store.prune(604800, u64::MAX, &[]),
        1,
        "the expired snapshot is removed even with a generous size cap"
    );
    assert!(!store_path.exists(), "the expired snapshot is gone");
    assert!(
        !store_path
            .with_file_name(format!(".{name}.complete"))
            .exists(),
        "the complete marker is removed with the snapshot"
    );
    assert!(
        !store_path.with_file_name(format!(".{name}.lease")).exists(),
        "the lease sentinel is removed with the snapshot"
    );
    remove_dir_all(&tmp).ok();
}

/// A directory with no complete marker predates the marker, so it ages out
/// like any other snapshot once its lease is free. While a live process
/// holds the lease, prune keeps it.
#[cfg(unix)]
#[test]
fn test_prune_reclaims_legacy() {
    let tmp = temp_dir().join(format!("snap-prune-legacy-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let store = SnapshotStore::new(&tmp).expect("store");
    let snaps = tmp.join(".houyicoder").join("snapshots");
    let legacy = snaps.join("snap-9");
    create_dir_all(&legacy).expect("legacy snapshot");
    write(legacy.join("old.txt"), "x").expect("legacy copy");
    let sentinel = snaps.join(".snap-9.lease");
    let held = claim_exclusive(&sentinel).expect("hold the lease");
    let directory = File::open(&legacy).expect("open the legacy directory");
    let aged = SystemTime::now() - Duration::from_secs(7200);
    directory
        .set_modified(aged)
        .expect("age the legacy directory");
    assert_eq!(
        store.prune(3600, u64::MAX, &[]),
        0,
        "a snapshot a live process holds is kept"
    );
    assert!(legacy.is_dir(), "the held snapshot stays on disk");
    drop(held);
    mark_legacy_sentinel(&sentinel).expect("tag the pre-marker snapshot");
    assert_eq!(
        store.prune(3600, u64::MAX, &[]),
        1,
        "a released legacy snapshot ages out"
    );
    assert!(!legacy.exists(), "the expired legacy snapshot is removed");
    assert!(!sentinel.exists(), "its lease sentinel is removed with it");
    remove_dir_all(&tmp).ok();
}

/// A store that fits the size cap loses nothing.
#[test]
fn test_prune_keeps_within_cap() {
    let tmp = temp_dir().join(format!("snap-prune-cap-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("write");
    let store = SnapshotStore::new(&tmp).expect("store");
    let entry = store.snapshot(&[]).expect("snapshot");
    drop(entry);
    assert_eq!(
        store.prune(604800, u64::MAX, &[]),
        0,
        "a store under the size cap keeps its snapshots"
    );
    remove_dir_all(&tmp).ok();
}

/// A store whose lock cannot be created is left alone rather than pruned
/// without serialization.
#[test]
fn test_relink_without_lock() {
    let tmp = temp_dir().join(format!("snap-relink-nolock-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let blocker = tmp.join("store-as-file");
    write(&blocker, "x").expect("write a file where the store belongs");
    let store = SnapshotStore::new_at(&tmp, &blocker).expect("store");
    assert!(
        store.relink_undo_entries().is_empty(),
        "a store that cannot be locked re-links nothing"
    );
    remove_dir_all(&tmp).ok();
}
