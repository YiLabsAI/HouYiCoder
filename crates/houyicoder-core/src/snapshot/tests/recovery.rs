//! Tests for recovery decisions: which tree a re-link, prune, or store move
//! accepts, keeps, or refuses when its sidecars disagree.

use std::env::temp_dir;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::metadata;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::Path;
use std::process::id;
use std::time::Duration;
use std::time::SystemTime;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use super::super::SnapshotStore;
use super::super::UndoEntry;
use super::super::lease::claim_exclusive;
use super::super::lease::mark_legacy_sentinel;
use super::super::lease::sentinel_path;
use super::super::prune::claim_for_delete;
use super::super::staging::complete_marker;

/// Age a directory far past every retention window.
fn age_directory(path: &Path) {
    let aged = SystemTime::now() - Duration::from_secs(7200);
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
    let handle = opts.open(path).expect("open snapshot dir");
    handle.set_modified(aged).expect("age the snapshot");
}

/// A markerless tree beside an empty sentinel was written by a publish that
/// never finished, or its marker was lost later. Prune keeps it rather than
/// deleting a tree it cannot tell apart from a half-written one.
#[test]
fn test_prune_skips_untagged_tree() {
    let tmp = temp_dir().join(format!("snap-prune-untagged-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let store = SnapshotStore::new(&tmp).expect("store");
    let snaps = tmp.join(".houyicoder").join("snapshots");
    let tree = snaps.join("snap-9");
    create_dir_all(&tree).expect("markerless snapshot");
    write(tree.join("partial.txt"), "x").expect("partial copy");
    let sentinel = sentinel_path(&snaps, "snap-9");
    let held = claim_exclusive(&sentinel).expect("write the empty sentinel");
    drop(held);
    age_directory(&tree);
    assert_eq!(
        store.prune(3600, u64::MAX, &[]),
        0,
        "an empty sentinel beside a markerless tree is not proof the tree is complete"
    );
    assert!(tree.is_dir(), "the unproven tree stays on disk");
    assert!(sentinel.exists(), "its sentinel stays with it");
    remove_dir_all(&tmp).ok();
}

/// A candidate is listed before the namespace lock is taken. A name that was
/// deleted and published again in between belongs to a newer snapshot, whose
/// lease must not be claimed on the older listing's behalf.
#[test]
fn test_prune_ignores_stale_candidate() {
    let tmp = temp_dir().join(format!("snap-prune-stale-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    let tree = tmp.join("snap-3");
    create_dir_all(&tree).expect("snapshot dir");
    write(tree.join("f.txt"), "x").expect("file");
    let seen = metadata(&tree)
        .and_then(|meta| meta.modified())
        .expect("snapshot mtime");
    assert!(
        claim_for_delete(&tree, seen).is_some(),
        "the unchanged directory is claimable"
    );
    assert!(
        claim_for_delete(&tree, seen - Duration::from_secs(1)).is_none(),
        "a directory whose mtime moved past the listing is not taken"
    );
    remove_dir_all(&tmp).ok();
}

/// A previous store that still holds a snapshot keeps its id reserved. The
/// new location must not publish a snapshot under a name a later start would
/// then have to skip, or that snapshot could never move.
#[test]
fn test_counter_skips_held_legacy() {
    let workspace = temp_dir().join(format!("snap-held-legacy-{}", id()));
    remove_dir_all(&workspace).ok();
    create_dir_all(&workspace).expect("workspace");
    write(workspace.join("real.txt"), "data").expect("file");
    let legacy = workspace.join(".houyicoder").join("snapshots");
    let held_snapshot = legacy.join("snap-7");
    create_dir_all(&held_snapshot).expect("legacy snapshot");
    write(complete_marker(&legacy, "snap-7"), b"").expect("marker");
    let held = claim_exclusive(&sentinel_path(&legacy, "snap-7")).expect("hold the snapshot");
    let new_root = temp_dir().join(format!("snap-held-legacy-out-{}", id()));
    remove_dir_all(&new_root).ok();
    let moved = SnapshotStore::new_at(&workspace, &new_root).expect("store");
    assert!(
        held_snapshot.is_dir(),
        "a snapshot another process holds stays in the previous store"
    );
    let next = moved.snapshot(&[]).expect("snapshot");
    let UndoEntry::CoWSnapshot { store_path, .. } = next else {
        panic!("expected CoWSnapshot");
    };
    assert_eq!(
        store_path.file_name().and_then(|name| name.to_str()),
        Some("snap-8"),
        "the counter skips the id the move could not take"
    );
    drop(held);
    remove_dir_all(&workspace).ok();
    remove_dir_all(&new_root).ok();
}

/// A markerless source beside an empty sentinel is not moved. The move would
/// carry an unproven tree into the current location and present it to a
/// re-link that refuses it, so it stays where it is for a manual recovery.
#[test]
fn test_move_skips_untagged_source() {
    let workspace = temp_dir().join(format!("snap-move-untagged-{}", id()));
    remove_dir_all(&workspace).ok();
    create_dir_all(&workspace).expect("workspace");
    write(workspace.join("real.txt"), "data").expect("file");
    let legacy = workspace.join(".houyicoder").join("snapshots");
    create_dir_all(legacy.join("snap-3")).expect("legacy snapshot");
    write(legacy.join("snap-3").join("f.txt"), "x").expect("file");
    let sentinel = sentinel_path(&legacy, "snap-3");
    let held = claim_exclusive(&sentinel).expect("write the empty sentinel");
    drop(held);
    let new_root = temp_dir().join(format!("snap-move-untagged-out-{}", id()));
    remove_dir_all(&new_root).ok();
    let moved = SnapshotStore::new_at(&workspace, &new_root).expect("store");
    assert!(
        legacy.join("snap-3").is_dir(),
        "the unproven snapshot stays in the previous store"
    );
    assert!(
        !new_root.join("snap-3").exists(),
        "nothing moves without a marker or a legacy tag"
    );
    assert!(
        moved.relink_undo_entries().is_empty(),
        "the new root holds no snapshot"
    );
    remove_dir_all(&workspace).ok();
    remove_dir_all(&new_root).ok();
}

/// A tagged sentinel left behind with no holder is the state a crash between
/// the tag write and the claim leaves. The next re-link accepts it as a
/// pre-marker snapshot, so the crash cannot make the tree permanently
/// unusable.
#[test]
fn test_relink_accepts_tagged() {
    let tmp = temp_dir().join(format!("snap-relink-tagged-{}", id()));
    remove_dir_all(&tmp).ok();
    create_dir_all(&tmp).expect("mkdir");
    write(tmp.join("real.txt"), "data").expect("file");
    let store = SnapshotStore::new(&tmp).expect("store");
    let snaps = tmp.join(".houyicoder").join("snapshots");
    let tree = snaps.join("snap-4");
    create_dir_all(&tree).expect("markerless snapshot");
    write(tree.join("old.txt"), "x").expect("copy");
    mark_legacy_sentinel(&sentinel_path(&snaps, "snap-4")).expect("tag the sentinel");
    let entries = store.relink_undo_entries();
    assert_eq!(
        entries.len(),
        1,
        "the tagged snapshot re-links: {entries:?}"
    );
    assert!(
        entries[0].description().contains("unverified"),
        "a pre-marker snapshot is never reported as verified"
    );
    remove_dir_all(&tmp).ok();
}

/// A lease handle reads its own sentinel even on a platform that locks the
/// file range against every other handle.
#[test]
fn test_lease_reads_own_sentinel() {
    let tmp = temp_dir().join(format!("snap-lease-read-{}", id()));
    remove_dir_all(&tmp).ok();
    let path = tmp.join(".snap-1.lease");
    let lease = claim_exclusive(&path).expect("claim");
    assert!(
        !lease.carries_legacy_tag(),
        "a fresh sentinel carries no tag"
    );
    remove_dir_all(&tmp).ok();
}
