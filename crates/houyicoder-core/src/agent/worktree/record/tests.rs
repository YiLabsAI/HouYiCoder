//! What a creation record and a worktree lease answer, including the cases a
//! caller must read as "not ours" or "in use".

use std::env::temp_dir;
use std::fs;
use std::fs::create_dir_all;
use std::fs::read_to_string;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::symlink;

use super::*;

static ROOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn temp_repo(tag: &str) -> PathBuf {
    let sequence = ROOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let dir = temp_dir().join(format!("houyi-wt-record-{tag}-{}-{sequence}", id()));
    drop(remove_dir_all(&dir));
    create_dir_all(&dir).expect("create temp repo");
    dir
}

#[test]
fn test_record_round_trips() {
    let repo = temp_repo("round-trip");
    let record = ManagedWorktree::new("feat-x", Some("dev".into()));
    write_managed_worktree(&repo, &record).expect("write record");

    let read = read_managed_worktree(&repo, "feat-x").expect("read record");
    assert_eq!(read.slug(), "feat-x");
    assert_eq!(read.branch(), "worktree-feat-x");
    assert_eq!(read.base_branch(), Some("dev"));
    assert_eq!(read.created_ms(), record.created_ms());
    drop(remove_dir_all(&repo));
}

/// A link where the state directory belongs is refused before anything is
/// written: records this product cannot place inside the repository would
/// otherwise be written, and cleared again, wherever the link led.
#[cfg(unix)]
#[test]
fn test_write_refuses_linked_state() {
    let repo = temp_repo("linked-state");
    create_dir_all(repo.join(".houyicoder")).expect("state parent");
    let outside = temp_repo("linked-state-out");
    symlink(&outside, state_dir(&repo)).expect("link the state directory");

    let error = write_managed_worktree(&repo, &ManagedWorktree::new("feat-x", None))
        .expect_err("the state directory resolves outside");

    assert!(
        matches!(error, WorktreeError::State { .. }),
        "expected State, got {error:?}"
    );
    assert!(
        !outside.join("feat-x.json").exists(),
        "nothing was written outside the repository"
    );
    drop(remove_dir_all(&repo));
    drop(remove_dir_all(&outside));
}

/// A record written under another schema is not read under today's rules: the
/// slug answers as refused, which makes a caller leave the directory alone.
#[test]
fn test_record_rejects_other_schema() {
    let repo = temp_repo("schema");
    let path = record_path(&repo, "old");
    create_dir_all(path.parent().expect("parent")).expect("create dir");
    write(
        &path,
        r#"{"schema":2,"slug":"old","branch":"worktree-old","base_branch":null,"created_ms":1}"#,
    )
    .expect("write record");

    assert!(matches!(
        lookup_record(&repo, "old"),
        RecordLookup::Rejected { .. }
    ));
    assert!(read_managed_worktree(&repo, "old").is_none());
    drop(remove_dir_all(&repo));
}

/// A record this version cannot read is refused, not read as absent: a caller
/// that could not tell the two apart would write a new record over the only
/// evidence of what the tree is. Bytes that do not parse and a shape with a
/// field this version does not know both answer Rejected.
#[test]
fn test_corrupt_record_is_rejected() {
    let repo = temp_repo("corrupt");
    assert!(matches!(lookup_record(&repo, "gone"), RecordLookup::Absent));
    let path = record_path(&repo, "gone");
    create_dir_all(path.parent().expect("parent")).expect("create dir");

    write(&path, "{not json").expect("write bytes");
    assert!(
        matches!(lookup_record(&repo, "gone"), RecordLookup::Rejected { .. }),
        "bytes that do not parse"
    );

    write(
        &path,
        r#"{"schema":1,"slug":"gone","branch":"worktree-gone","base_branch":null,"created_ms":1,"owner":"x"}"#,
    )
    .expect("write an unknown field");
    assert!(
        matches!(lookup_record(&repo, "gone"), RecordLookup::Rejected { .. }),
        "a field this version does not know"
    );
    drop(remove_dir_all(&repo));
}

/// A record file whose slug names another worktree answers as refused for the
/// slug asked about, so a copied or renamed file does not adopt a tree.
#[test]
fn test_record_rejects_other_slug() {
    let repo = temp_repo("slug");
    let record = ManagedWorktree::new("alpha", None);
    write_managed_worktree(&repo, &record).expect("write record");
    let alpha = record_path(&repo, "alpha");
    let beta = record_path(&repo, "beta");
    fs::copy(&alpha, &beta).expect("copy record");

    assert!(matches!(
        lookup_record(&repo, "beta"),
        RecordLookup::Rejected { .. }
    ));
    assert!(read_managed_worktree(&repo, "beta").is_none());
    assert!(read_managed_worktree(&repo, "alpha").is_some());
    drop(remove_dir_all(&repo));
}

/// A record naming a branch this product does not derive from the slug is
/// refused here rather than accepted and refused later by the identity check:
/// two answers to "is this tree ours" would let the store and the removal
/// disagree about the same record.
#[test]
fn test_record_rejects_foreign_branch() {
    let repo = temp_repo("branch");
    let path = record_path(&repo, "alpha");
    create_dir_all(path.parent().expect("parent")).expect("create dir");
    write(
        &path,
        r#"{"schema":1,"slug":"alpha","branch":"worktree-beta","base_branch":null,"created_ms":1}"#,
    )
    .expect("write record");

    assert!(matches!(
        lookup_record(&repo, "alpha"),
        RecordLookup::Rejected { .. }
    ));
    drop(remove_dir_all(&repo));
}

/// The record is published whole: no temporary file survives a successful
/// write, and a second write replaces the first.
#[test]
fn test_record_replaces_whole() {
    let repo = temp_repo("replace");
    write_managed_worktree(&repo, &ManagedWorktree::new("x", Some("main".into())))
        .expect("first write");
    write_managed_worktree(&repo, &ManagedWorktree::new("x", Some("dev".into())))
        .expect("second write");

    let read = read_managed_worktree(&repo, "x").expect("read record");
    assert_eq!(read.base_branch(), Some("dev"), "the second write stands");
    let leftovers: Vec<_> = fs::read_dir(state_dir(&repo))
        .expect("list records")
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "no temporary file is left behind");
    drop(remove_dir_all(&repo));
}

/// A nested slug flattens into one file name inside the state directory, so
/// no record path can climb out of it.
#[test]
fn test_record_path_escapes_nothing() {
    let repo = temp_repo("path");
    let record = ManagedWorktree::new("user/feature", None);
    write_managed_worktree(&repo, &record).expect("write record");

    let path = record_path(&repo, "user/feature");
    assert_eq!(path.parent(), Some(state_dir(&repo).as_path()));
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some("user+feature.json")
    );
    assert!(read_managed_worktree(&repo, "user/feature").is_some());
    drop(remove_dir_all(&repo));
}

/// One holder at a time: a second lease on the same worktree is refused while
/// the first is held, and the tree reads as idle once it drops.
#[test]
fn test_lease_admits_one_holder() {
    let repo = temp_repo("lease");
    let lease = WorktreeLease::acquire(&repo, "busy").expect("first lease");
    assert!(
        worktree_leased(&repo, "busy").expect("probe the lease"),
        "the holder is visible"
    );

    let second = WorktreeLease::acquire(&repo, "busy");
    assert!(
        matches!(second, Err(WorktreeError::Leased { .. })),
        "a second holder is refused: {second:?}"
    );

    drop(lease);
    assert!(
        !worktree_leased(&repo, "busy").expect("probe the lease"),
        "the lease is free again"
    );
    assert!(WorktreeLease::acquire(&repo, "busy").is_ok());
    drop(remove_dir_all(&repo));
}

/// A worktree nobody leased reads as idle without creating a file, so probing
/// a list of worktrees does not leave a marker in each one.
#[test]
fn test_lease_absent_is_idle() {
    let repo = temp_repo("idle");
    assert!(!worktree_leased(&repo, "fresh").expect("probe the lease"));
    let leases: Vec<_> = fs::read_dir(state_dir(&repo))
        .map(|entries| entries.flatten().collect())
        .unwrap_or_default();
    assert!(leases.is_empty(), "the probe created nothing");
    drop(remove_dir_all(&repo));
}

/// A lease path occupied by a directory cannot be read or locked, so the
/// answer is an error rather than "idle": a removal that read an unreadable
/// lease as free would delete a tree a session may be working in.
#[test]
fn test_unreadable_lease_is_unknown() {
    let repo = temp_repo("unreadable");
    create_dir_all(lease_path(&repo, "broken")).expect("occupy the lease path");

    let probe = worktree_leased(&repo, "broken");
    assert!(
        matches!(probe, Err(WorktreeError::LeaseUnknown { .. })),
        "an unreadable lease is unknown: {probe:?}"
    );
    let acquired = WorktreeLease::acquire(&repo, "broken");
    assert!(
        matches!(acquired, Err(WorktreeError::LeaseUnknown { .. })),
        "taking it is refused for the same reason: {acquired:?}"
    );
    drop(remove_dir_all(&repo));
}

/// A slug outside the allowlist cannot name a record this product wrote, so the
/// store answers refused rather than absent: nothing acts on a directory under
/// a name no record path describes.
#[test]
fn test_foreign_slug_record_rejected() {
    let repo = temp_repo("foreign-slug");
    assert!(matches!(
        lookup_record(&repo, "../escape"),
        RecordLookup::Rejected { .. }
    ));
    drop(remove_dir_all(&repo));
}

/// A record path occupied by a directory cannot be read as a record, and that
/// answers refused: a removal that read it as absent would act on a tree whose
/// evidence is still sitting there.
#[test]
fn test_unreadable_record_rejected() {
    let repo = temp_repo("unreadable-record");
    create_dir_all(record_path(&repo, "taken")).expect("occupy the record path");

    assert!(matches!(
        lookup_record(&repo, "taken"),
        RecordLookup::Rejected { .. }
    ));
    drop(remove_dir_all(&repo));
}

/// A state directory occupied by a file holds neither a record nor a lease, and
/// both writers name the path they could not create instead of failing later at
/// a step that reads as unrelated.
#[test]
fn test_blocked_state_dir_refused() {
    let repo = temp_repo("blocked-state");
    write(repo.join(".houyicoder"), "not a directory").expect("occupy the state dir");

    let stored = write_managed_worktree(&repo, &ManagedWorktree::new("x", None));
    assert!(
        matches!(stored, Err(WorktreeError::State { .. })),
        "the record is refused: {stored:?}"
    );
    let leased = WorktreeLease::acquire(&repo, "x");
    assert!(
        matches!(leased, Err(WorktreeError::State { .. })),
        "the lease is refused: {leased:?}"
    );
    drop(remove_dir_all(&repo));
}

/// The bytes on disk name the record, for an operator reading the store.
#[test]
fn test_record_file_is_readable() {
    let repo = temp_repo("bytes");
    write_managed_worktree(&repo, &ManagedWorktree::new("y", None)).expect("write record");
    let text = read_to_string(record_path(&repo, "y")).expect("read record file");
    assert!(
        text.contains("\"slug\":\"y\""),
        "the slug is on disk: {text}"
    );
    drop(remove_dir_all(&repo));
}
