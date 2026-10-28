//! The cleanup record's round trip, and what the guarded directory removal
//! refuses to reach.

use std::env::temp_dir;
use std::fs;
use std::process::id;
#[cfg(unix)]
use std::{fs::symlink_metadata, os::unix::fs::symlink};

use super::super::record::pending_path;
#[cfg(unix)]
use super::super::record::state_dir;
use super::*;

/// A throwaway directory under the temp dir, empty before the test starts.
fn scratch(tag: &str) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-cleanup-{tag}-{}", id()));
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A link inside the tree is dropped by its own name: the file system removes
/// the link, and the directory it names, which sits outside the tree entirely,
/// keeps its contents.
#[cfg(unix)]
#[test]
fn test_remove_tree_drops_link() {
    let base = scratch("link");
    let outside = base.join("outside");
    let tree = base.join("tree");
    fs::create_dir_all(&outside).expect("outside dir");
    fs::write(outside.join("keep.txt"), "keep").expect("outside file");
    fs::create_dir_all(tree.join("nested")).expect("nested dir");
    fs::write(tree.join("nested").join("work.txt"), "work").expect("tree file");
    symlink(&outside, tree.join("link")).expect("link into the tree");

    remove_tree(&tree, None).expect("the tree is removed");

    assert!(!tree.exists(), "the tree is gone");
    assert!(
        outside.join("keep.txt").exists(),
        "what the link named is untouched"
    );
    drop(fs::remove_dir_all(&base));
}

/// A path that is not a directory is refused rather than removed: the caller
/// reached this point believing it held a leftover tree, and a file is not one.
#[test]
fn test_remove_tree_refuses_file() {
    let base = scratch("file");
    let file = base.join("plain.txt");
    fs::write(&file, "keep").expect("write file");

    assert!(
        remove_tree(&file, None).is_err(),
        "a file is not a directory"
    );
    assert!(file.exists(), "the file is left alone");
    drop(fs::remove_dir_all(&base));
}

/// A link standing where the tree should be is refused, not followed: removing
/// it would otherwise delete whatever directory it names.
#[cfg(unix)]
#[test]
fn test_link_root_refused() {
    let base = scratch("link-root");
    let outside = base.join("outside");
    fs::create_dir_all(&outside).expect("outside dir");
    fs::write(outside.join("keep.txt"), "keep").expect("outside file");
    let link = base.join("tree");
    symlink(&outside, &link).expect("link where the tree goes");

    assert!(remove_tree(&link, None).is_err(), "a link is not the tree");
    assert!(
        outside.join("keep.txt").exists(),
        "the directory the link names keeps its contents"
    );
    assert!(
        symlink_metadata(&link).is_ok(),
        "the link itself is left in place"
    );
    drop(fs::remove_dir_all(&base));
}

/// A directory replaced between the checks and the walk is refused: the walk
/// compares the numbers the caller read before it deletes anything.
#[cfg(unix)]
#[test]
fn test_remove_tree_refuses_replaced() {
    let base = scratch("replaced");
    let tree = base.join("tree");
    fs::create_dir_all(tree.join("nested")).expect("nested dir");
    let stamp = TargetStamp::read(&tree)
        .expect("read the stamp")
        .expect("the tree is there");
    fs::rename(&tree, base.join("moved")).expect("move the directory away");
    fs::create_dir_all(&tree).expect("a new directory in its place");
    fs::write(tree.join("keep.txt"), "keep").expect("a file in the replacement");

    assert!(
        remove_tree(&tree, Some(stamp)).is_err(),
        "the replacement is refused"
    );
    assert!(
        tree.join("keep.txt").exists(),
        "the replacement is untouched"
    );
    drop(fs::remove_dir_all(&base));
}

/// A link standing where the cleanup records belong is refused rather than
/// followed: a record written through it would land outside the state
/// directory, and a later removal would delete a file named there.
#[cfg(unix)]
#[test]
fn test_pending_refuses_linked_dir() {
    let repo = scratch("linked-dir");
    let outside = repo.join("outside");
    fs::create_dir_all(&outside).expect("outside dir");
    let state = state_dir(&repo);
    fs::create_dir_all(&state).expect("state dir");
    symlink(&outside, state.join("pending")).expect("link where the records go");

    let error = write_pending(
        &repo,
        &PendingCleanup::new(
            "feat",
            &repo.join("leftover"),
            None,
            RemovalIntent::Conservative,
        ),
    )
    .expect_err("the record directory resolves outside the state directory");
    assert!(
        matches!(error, WorktreeError::State { .. }),
        "expected State, got {error:?}"
    );
    assert!(
        lookup_pending(&repo, "feat").is_err(),
        "reading through the link is refused too"
    );
    clear_pending(&repo, "feat");
    assert_eq!(
        fs::read_dir(&outside).expect("read outside").count(),
        0,
        "nothing was written or deleted outside the state directory"
    );
    drop(fs::remove_dir_all(&repo));
}

/// The record names the target it was written for, so a later attempt does not
/// have to guess from the slug alone.
#[test]
fn test_pending_round_trips() {
    let repo = scratch("record");
    let path = repo.join("leftover");
    let pending = PendingCleanup::new("feat", &path, None, RemovalIntent::KeepBranch);

    write_pending(&repo, &pending).expect("write the record");
    let read = lookup_pending(&repo, "feat")
        .expect("read the record")
        .expect("a record is there");

    assert_eq!(read.slug(), "feat");
    assert_eq!(read.path(), path);
    assert_eq!(
        read.intent(),
        RemovalIntent::KeepBranch,
        "the record keeps what the removal was allowed to do"
    );

    clear_pending(&repo, "feat");
    assert!(
        lookup_pending(&repo, "feat")
            .expect("read after clear")
            .is_none(),
        "a cleared record is absent rather than refused"
    );
    // Clearing a record that is already gone is not a failure: the removal
    // that finishes a leftover does not know whether an earlier attempt
    // managed to write one.
    clear_pending(&repo, "feat");
    drop(fs::remove_dir_all(&repo));
}

/// A record written before the intent was kept finishes conservatively: an
/// authorization this version cannot read is not one it may act on, and
/// Git's own caution is the answer that loses nothing.
#[test]
fn test_old_record_stays_conservative() {
    let repo = scratch("old-record");
    let path = pending_path(&repo, "legacy");
    fs::create_dir_all(path.parent().expect("state dir")).expect("state dir");
    fs::write(
        &path,
        r#"{"schema":1,"slug":"legacy","path":"/left","stamp":null}"#,
    )
    .expect("write a record from an earlier version");

    let read = lookup_pending(&repo, "legacy")
        .expect("read the record")
        .expect("a record is there");

    assert_eq!(
        read.intent(),
        RemovalIntent::Conservative,
        "a record that names no intent overrides nothing"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A record from a later version is refused rather than read with fields
/// dropped: the field this version does not know could be the one naming the
/// target.
#[test]
fn test_pending_refuses_other_schema() {
    let repo = scratch("schema");
    let path = pending_path(&repo, "future");
    fs::create_dir_all(path.parent().expect("state dir")).expect("state dir");
    fs::write(&path, r#"{"schema":2,"slug":"future","path":"/left"}"#)
        .expect("write a record from a later version");

    let error = lookup_pending(&repo, "future").expect_err("the record is refused");
    assert!(
        matches!(error, WorktreeError::State { .. }),
        "expected State, got {error:?}"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A record that does not parse is refused rather than reported absent: a
/// damaged record still names a leftover, and reporting absence would drop it.
#[test]
fn test_pending_refuses_malformed() {
    let repo = scratch("malformed");
    let path = pending_path(&repo, "damaged");
    fs::create_dir_all(path.parent().expect("state dir")).expect("state dir");
    fs::write(&path, "{ not a record").expect("write a damaged record");

    let error = lookup_pending(&repo, "damaged").expect_err("the record is refused");
    assert!(
        matches!(error, WorktreeError::State { .. }),
        "expected State, got {error:?}"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A record that cannot be read is refused rather than reported absent:
/// absence would let a removal take the ordinary path and report a failure
/// that hides the leftover nobody can then finish.
#[test]
fn test_pending_refuses_unreadable() {
    let repo = scratch("unreadable");
    let path = pending_path(&repo, "stuck");
    fs::create_dir_all(&path).expect("a directory where the record goes");

    let error = lookup_pending(&repo, "stuck").expect_err("the record is refused");
    assert!(
        matches!(error, WorktreeError::State { .. }),
        "expected State, got {error:?}"
    );

    clear_pending(&repo, "stuck");
    assert!(
        path.exists(),
        "a record that could not be cleared is left in place"
    );
    drop(fs::remove_dir_all(&repo));
}
