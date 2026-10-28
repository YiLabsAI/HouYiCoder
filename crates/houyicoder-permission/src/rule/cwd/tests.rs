//! The leading-move rule spelling: which commands reduce to the root's work.

use super::root_relative_content;
use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all};
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn scratch_root(label: &str) -> PathBuf {
    let path = temp_dir().join(format!(
        "hy-rule-cwd-{label}-{}-{}",
        process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    create_dir_all(&path).expect("mkdir scratch");
    path
}

#[test]
fn test_move_into_root_strips() {
    let root = scratch_root("inside");
    let absolute = format!("cd {} && git status", root.display());
    assert_eq!(
        root_relative_content(&absolute, &root).as_deref(),
        Some("git status")
    );
    assert_eq!(
        root_relative_content("cd . && git status", &root).as_deref(),
        Some("git status")
    );
    remove_dir_all(&root).expect("rm scratch");
}

#[test]
fn test_move_outside_root_ignored() {
    let root = scratch_root("root");
    let other = scratch_root("other");
    let absolute = format!("cd {} && git status", other.display());
    assert_eq!(root_relative_content(&absolute, &root), None);
    // A separator other than and-then leaves the rest of the command running
    // even when the move fails, so that rest is not the root's work.
    let semicolon = format!("cd {}; git status", root.display());
    assert_eq!(root_relative_content(&semicolon, &root), None);
    assert_eq!(root_relative_content("cd .. && git status", &root), None);
    assert_eq!(root_relative_content("git status", &root), None);
    remove_dir_all(&root).expect("rm scratch");
    remove_dir_all(&other).expect("rm scratch");
}

#[test]
fn test_dynamic_target_ignored() {
    let root = scratch_root("dynamic");
    for command in [
        "cd $HOME && git status",
        "cd $(pwd) && git status",
        "cd `pwd` && git status",
    ] {
        assert_eq!(root_relative_content(command, &root), None, "{command}");
    }
    remove_dir_all(&root).expect("rm scratch");
}
