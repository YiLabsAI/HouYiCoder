//! How a status listing is split into the classes a removal reports.

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::id;

use super::*;

/// A throwaway repository, empty, so every file written into it is untracked.
fn make_repo(tag: &str) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-wt-changes-{tag}-{}", id()));
    drop(remove_dir_all(&dir));
    create_dir_all(&dir).expect("repo dir");
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@x"]);
    git(&dir, &["config", "user.name", "t"]);
    dir
}

#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding, not an engine spawn"
)]
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// A rename record names its source on a second field; that field is not a
/// second changed file.
#[test]
fn test_classify_counts_rename_once() {
    let raw = b"R  new.txt\0old.txt\0 M kept.txt\0";
    let changes = classify_changes(raw);
    assert_eq!(changes.tracked_dirty, 2, "{changes:?}");
}

/// A path holding a newline is one record: a reader that split the listing
/// into lines would read the same file as one changed entry and one untracked
/// entry, so every class is asserted.
#[test]
fn test_classify_keeps_newline_path() {
    let raw = b"?? a\nb.txt\0";
    let changes = classify_changes(raw);
    assert_eq!(changes.tracked_dirty, 0, "{changes:?}");
    assert_eq!(changes.untracked, 1, "{changes:?}");
    assert_eq!(changes.ignored_unknown, 0, "{changes:?}");
    assert!(changes.ignored_build_dirs.is_empty(), "{changes:?}");
}

/// The same name read from Git's own listing: the status call keeps a path
/// holding a newline in one record, so the file is counted once.
#[cfg(unix)]
#[test]
fn test_status_keeps_newline_name() {
    let repo = make_repo("newline");
    write(repo.join("a\nb.txt"), "x\n").expect("write a name with a newline");

    let changes = read_changes(&repo).expect("read changes");
    assert_eq!(changes.tracked_dirty, 0, "{changes:?}");
    assert_eq!(changes.untracked, 1, "{changes:?}");
    drop(remove_dir_all(&repo));
}

/// Git reports an ignored directory once, at whatever depth it sits, and the
/// entry ends in a slash. A nested node_modules is the same regenerable output
/// as one at the root, so it is not counted as work whose content nobody knows,
/// and the report keeps the path so the user learns where it is.
#[test]
fn test_classify_nested_build_dir() {
    let raw = b"!! packages/a/node_modules/\0!! target/\0";
    let changes = classify_changes(raw);

    assert_eq!(changes.ignored_unknown, 0, "{changes:?}");
    assert_eq!(
        changes.ignored_build_dirs,
        vec!["packages/a/node_modules", "target"],
        "{changes:?}"
    );
}

/// An ignored file that carries a build name is not build output: Git marks a
/// directory with a trailing slash and a file without one, and a file counted
/// as regenerable output would leave the gate without the user hearing what it
/// held.
#[test]
fn test_ignored_file_not_build() {
    let raw = b"!! build\0!! dist/\0";
    let changes = classify_changes(raw);

    assert_eq!(changes.ignored_unknown, 1, "{changes:?}");
    assert_eq!(changes.ignored_build_dirs, vec!["dist"], "{changes:?}");
}

/// The same nested directory read from Git's own listing, so the classifier's
/// account of the entry shape is measured rather than assumed.
#[test]
fn test_status_finds_nested_build() {
    let repo = make_repo("nested-build");
    write(repo.join(".gitignore"), "node_modules/\n").expect("write ignore rules");
    git(&repo, &["add", ".gitignore"]);
    git(&repo, &["commit", "-qm", "ignore rules"]);
    create_dir_all(repo.join("packages/a/node_modules/tool")).expect("nested output");
    write(repo.join("packages/a/node_modules/tool/x.js"), "x\n").expect("output file");

    let changes = read_changes(&repo).expect("read changes");
    assert_eq!(changes.ignored_unknown, 0, "{changes:?}");
    assert_eq!(
        changes.ignored_build_dirs,
        vec!["packages/a/node_modules"],
        "{changes:?}"
    );
    drop(remove_dir_all(&repo));
}

/// A file written into a fresh repository is counted from the real listing,
/// including a name holding a space.
#[test]
fn test_status_counts_untracked_file() {
    let repo = make_repo("listed");
    write(repo.join("a b.txt"), "x\n").expect("write a name with a space");

    let changes = read_changes(&repo).expect("read changes");
    assert_eq!(changes.untracked, 1, "{changes:?}");
    drop(remove_dir_all(&repo));
}
