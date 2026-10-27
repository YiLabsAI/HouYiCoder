//! What a managed path has to be before a resume or a removal acts on it, and
//! how Git's own worktree listing is read.

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::symlink;

use super::*;

static ROOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A throwaway directory with the managed root in place, so the checks that do
/// not need Git can run without one.
fn temp_repo(tag: &str) -> PathBuf {
    let sequence = ROOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let dir = temp_dir().join(format!("houyi-wt-identify-{tag}-{}-{sequence}", id()));
    drop(remove_dir_all(&dir));
    create_dir_all(worktrees_dir(&dir)).expect("managed root");
    dir
}

/// A slug whose directory is not there is refused rather than described: the
/// caller would otherwise act on a path nothing occupies.
#[test]
fn test_identity_refuses_missing() {
    let repo = temp_repo("missing");
    let error = resolve_identity(&repo, "gone", "worktree-gone").expect_err("nothing is there");
    assert!(
        matches!(error, WorktreeError::Identity { .. }),
        "expected Identity, got {error:?}"
    );
    drop(remove_dir_all(&repo));
}

/// A file at the managed path is not a worktree either, and the refusal says
/// so instead of letting a later Git call report it.
#[test]
fn test_identity_refuses_file() {
    let repo = temp_repo("file");
    write(worktree_path_for(&repo, "taken"), "x").expect("occupy the managed path");

    let error = resolve_identity(&repo, "taken", "worktree-taken").expect_err("a file is not one");
    assert!(
        matches!(error, WorktreeError::Identity { .. }),
        "expected Identity, got {error:?}"
    );
    drop(remove_dir_all(&repo));
}

/// A plain directory at the managed path is not a worktree, so it is refused
/// as such instead of letting the first Git call report whatever it reports.
#[test]
fn test_identity_refuses_plain_dir() {
    let repo = temp_repo("plain");
    create_dir_all(worktree_path_for(&repo, "plain")).expect("occupy the managed path");

    let error = resolve_identity(&repo, "plain", "worktree-plain").expect_err("no worktree there");
    assert!(
        matches!(error, WorktreeError::Identity { .. }),
        "expected Identity, got {error:?}"
    );
    drop(remove_dir_all(&repo));
}

/// The managed root is anchored to the repository rather than to itself: a
/// link standing where the root belongs moves every tree made under it outside
/// the repository, and Git registers a worktree there as readily as anywhere
/// else. The tree is refused because the root it sits under is not the root of
/// the repository that asked.
#[cfg(unix)]
#[test]
fn test_identity_refuses_linked_root() {
    let sequence = ROOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let repo = temp_dir().join(format!("houyi-wt-linked-{}-{sequence}", id()));
    drop(remove_dir_all(&repo));
    create_dir_all(&repo).expect("repo dir");
    run_git(&repo, &["init", "-q"]).expect("init");
    run_git(&repo, &["config", "user.email", "t@x"]).expect("user email");
    run_git(&repo, &["config", "user.name", "t"]).expect("user name");
    run_git(&repo, &["commit", "--allow-empty", "-m", "init", "-q"]).expect("first commit");
    create_dir_all(repo.join(".houyicoder")).expect("state dir");
    let outside = temp_dir().join(format!("houyi-wt-linked-out-{}-{sequence}", id()));
    drop(remove_dir_all(&outside));
    create_dir_all(&outside).expect("outside dir");
    symlink(&outside, worktrees_dir(&repo)).expect("link the managed root");
    let expected = worktree_path_for(&repo, "linked");
    run_git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "worktree-linked",
            &expected.to_string_lossy(),
        ],
    )
    .expect("register the tree through the link");

    let error = resolve_identity(&repo, "linked", "worktree-linked").expect_err("root is outside");
    assert!(
        matches!(error, WorktreeError::Identity { .. }),
        "expected Identity, got {error:?}"
    );
    assert!(
        error.to_string().contains("resolves outside"),
        "the refusal names the root rather than a later check: {error}"
    );
    drop(remove_dir_all(&repo));
    drop(remove_dir_all(&outside));
}

/// Git's listing is read as NUL-separated records, so a path holding a newline
/// stays one path, and a lock line marks the entry it follows.
#[test]
fn test_listing_reads_lock() {
    let raw = b"worktree /a\nb\0HEAD 1\0branch refs/heads/x\0\0worktree /c\0locked why\0HEAD 2\0";
    let entries = parse_worktree_listing(raw);

    assert_eq!(entries.len(), 2, "one entry per worktree field");
    assert_eq!(entries[0].path, PathBuf::from("/a\nb"));
    assert!(!entries[0].locked, "the first entry carries no lock");
    assert_eq!(entries[1].path, PathBuf::from("/c"));
    assert!(entries[1].locked, "the lock line marks the entry before it");
}
