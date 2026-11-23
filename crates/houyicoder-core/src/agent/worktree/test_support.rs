//! A throwaway git repository for the worktree tests.
//!
//! One template repository is built per test binary, and each test takes a
//! file copy of it instead of running four git commands. Process creation is
//! the expensive part of the fixture on hosts where it is slow, and it cost
//! more than the code under test in every one of these tests.

use std::env::temp_dir;
use std::fmt::Display;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::id;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// The template: an empty commit on the default branch, with the identity the
/// fixture's commits use.
///
/// One template lives for the test binary's lifetime, so a run leaves one
/// directory in the temp root for the OS to reap rather than one per test.
/// The repository is built with no template directory of its own: a
/// developer's global git template can carry hooks and symlinks, and a copy
/// of those would either fail the file copy or run in every fixture.
fn template() -> &'static Path {
    static TEMPLATE: OnceLock<PathBuf> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let dir = temp_dir().join(format!("houyi-wt-template-{}", id()));
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(&dir).expect("create the template dir");
        for args in [
            ["-c", "init.templateDir=", "init", "-q"].as_slice(),
            ["config", "user.email", "t@x"].as_slice(),
            ["config", "user.name", "t"].as_slice(),
            ["commit", "--allow-empty", "-m", "init", "-q"].as_slice(),
        ] {
            git(&dir, args);
        }
        dir
    })
}

/// A fresh repository for one test: a copy of the template, so the test owns
/// its working tree, index, and refs.
pub(crate) fn make_repo(tag: impl Display) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = temp_dir().join(format!("houyi-wt-{tag}-{}-{n}", id()));
    drop(fs::remove_dir_all(&dir));
    copy_tree(template(), &dir);
    dir
}

/// Copy a directory tree. The template holds only small files, so a plain
/// recursive copy is faster than any spawn and needs no extra dependency.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create the copy dir");
    for entry in fs::read_dir(from).expect("read the template") {
        let entry = entry.expect("template entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("template file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy a template file");
        }
    }
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
