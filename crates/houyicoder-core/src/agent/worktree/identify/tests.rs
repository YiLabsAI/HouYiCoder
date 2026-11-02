//! What a managed path has to be before a resume or a removal acts on it, and
//! how Git's own worktree listing is read.

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::{fs::rename, os::unix::fs::symlink};

use super::super::session::worktree_branch_name;
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

/// A throwaway repository with one commit, so Git can register worktrees
/// against it.
fn temp_git_repo(tag: &str) -> PathBuf {
    let sequence = ROOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let dir = temp_dir().join(format!("houyi-wt-target-{tag}-{}-{sequence}", id()));
    drop(remove_dir_all(&dir));
    create_dir_all(&dir).expect("repo dir");
    run_git(&dir, &["init", "-q"]).expect("init");
    run_git(&dir, &["config", "user.email", "t@x"]).expect("user email");
    run_git(&dir, &["config", "user.name", "t"]).expect("user name");
    run_git(&dir, &["commit", "--allow-empty", "-m", "init", "-q"]).expect("first commit");
    dir
}

/// Register a linked worktree at a path Git takes as given, the way another
/// tool would place one.
fn linked_tree(repo: &Path, path: &Path, branch: &str) {
    create_dir_all(path.parent().expect("parent dir")).expect("tree parent");
    run_git(
        repo,
        &["worktree", "add", "-b", branch, &path.to_string_lossy()],
    )
    .expect("register the tree");
}

/// The repository's own working tree is named as such rather than as a linked
/// tree, so a caller can refuse to take it apart before it reads anything.
#[test]
fn test_target_names_main_checkout() {
    let repo = temp_git_repo("main");
    let target = resolve_target(&repo, &repo).expect("resolve the repository itself");

    assert!(target.is_main(), "the first listing entry is the main tree");
    assert_eq!(target.path(), canonicalize(&repo).expect("resolved repo"));
    assert_eq!(
        target.state_key(),
        format!(
            "@{:016x}",
            fnv1a(target.path().as_os_str().as_encoded_bytes())
        ),
        "a tree with no slug is named by a digest of its path"
    );
    drop(remove_dir_all(&repo));
}

/// A tree created elsewhere is a linked tree with no slug, whatever directory
/// it lives in, so the caller reads it as one this product did not create.
#[test]
fn test_target_marks_foreign_tree() {
    let repo = temp_git_repo("foreign");
    let path = repo.join(".claude").join("worktrees").join("feat");
    linked_tree(&repo, &path, "feat");

    let target = resolve_target(&repo, &path).expect("resolve a tree another tool made");

    assert!(!target.is_main());
    assert_eq!(target.role, WorktreeRole::Linked { slug: None });
    assert_eq!(target.name(), "feat");
    assert_eq!(target.identity.branch, "feat");
    drop(remove_dir_all(&repo));
}

/// A tree where this product creates its own is named by its slug, which is
/// what the creation record and the lease are keyed by.
#[test]
fn test_target_marks_managed_tree() {
    let repo = temp_git_repo("managed");
    create_dir_all(repo.join(".houyicoder")).expect("state dir");
    let path = worktree_path_for(&repo, "team/otter");
    linked_tree(&repo, &path, &worktree_branch_name("team/otter"));

    let target = resolve_target(&repo, &path).expect("resolve a tree this product made");

    assert_eq!(
        target.role,
        WorktreeRole::Linked {
            slug: Some("team/otter".to_string())
        }
    );
    assert_eq!(
        flatten_slug("team/otter"),
        target.state_key(),
        "the flattened slug names the state files"
    );
    assert_eq!(
        target.name(),
        "team+otter",
        "the pane names the directory, not the slug it came from"
    );
    drop(remove_dir_all(&repo));
}

/// A directory inside the repository that Git never registered is not a
/// worktree, and the refusal says so instead of describing it.
#[test]
fn test_target_refuses_plain_dir() {
    let repo = temp_git_repo("plain-dir");
    let path = repo.join("just-a-directory");
    create_dir_all(&path).expect("plain dir");

    let error = resolve_target(&repo, &path).expect_err("nothing registered it");

    assert!(
        matches!(error, WorktreeError::Identity { .. }),
        "expected Identity, got {error:?}"
    );
    drop(remove_dir_all(&repo));
}

/// A path that cannot be read has no numbers to hand, which is refused rather
/// than read as a directory that is not there.
#[test]
fn test_stamp_read_refuses_file() {
    let repo = temp_repo("stamp-file");
    let file = repo.join("plain.txt");
    write(&file, "keep").expect("write file");

    let error = TargetStamp::read(&file.join("child")).expect_err("the path cannot be read");

    assert!(
        matches!(error, WorktreeError::State { .. }),
        "expected State, got {error:?}"
    );
    // A file that resolves on its own is refused on the same terms: the stamp
    // answers for a directory, and a path that is not one has no numbers.
    let direct = TargetStamp::read(&file).expect_err("a file is not a directory");
    assert!(
        matches!(direct, WorktreeError::State { .. }),
        "expected State, got {direct:?}"
    );
    drop(remove_dir_all(&repo));
}

/// A path whose parent names a file is one no read could resolve, whatever
/// error kind the platform reports for it: the check reads the parent, so it
/// answers the same on every platform rather than following the platform's
/// own mapping of an unresolvable path.
#[cfg(windows)]
#[test]
fn test_parent_names_file() {
    let repo = temp_repo("parent-file");
    let file = repo.join("plain.txt");
    write(&file, "keep").expect("write file");
    let dir = repo.join("subdir");
    create_dir_all(&dir).expect("directory");

    assert!(
        parent_names_file(&file.join("child")),
        "a path under a file is unresolvable"
    );
    assert!(
        !parent_names_file(&dir.join("child")),
        "a path under a directory resolves"
    );
    drop(remove_dir_all(&repo));
}

/// A path with nothing at it has no numbers to hand, which is what tells a
/// removal that the directory it verified is not there any more.
#[test]
fn test_stamp_when_dir_gone() {
    let repo = temp_repo("stamp-gone");
    let path = repo.join("target");
    create_dir_all(&path).expect("target dir");
    let stamp = TargetStamp::read(&path)
        .expect("read the stamp")
        .expect("the directory is there");
    drop(remove_dir_all(&path));
    assert!(
        TargetStamp::read(&path)
            .expect("read a missing path")
            .is_none(),
        "a missing directory has no numbers"
    );

    assert!(stamp.verify(&path).is_err(), "the directory is gone");
    drop(remove_dir_all(&repo));
}

/// A directory that replaces the verified one is refused: the numbers a file
/// system gives a directory are what tell the two apart.
#[cfg(unix)]
#[test]
fn test_stamp_detects_replacement() {
    let repo = temp_repo("stamp-replaced");
    let path = repo.join("target");
    create_dir_all(&path).expect("target dir");
    let stamp = TargetStamp::read(&path)
        .expect("read the stamp")
        .expect("the directory is there");
    rename(&path, repo.join("moved")).expect("move the directory away");
    create_dir_all(&path).expect("a new directory in its place");

    assert!(
        stamp.verify(&path).is_err(),
        "the replacement is not the directory that was checked"
    );
    drop(remove_dir_all(&repo));
}
