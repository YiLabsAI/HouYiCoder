use super::super::parse_worktrees;
use std::env::temp_dir;
use std::fs::canonicalize;
use std::fs::remove_dir_all;
use std::process::Command;
use std::process::id;

fn make_repo(tag: u64) -> String {
    let dir = temp_dir().join(format!("houyi-wt-display-{}-{tag}", id()));
    drop(remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).expect("mkdir");
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "-q", "--initial-branch=main"])
            .status(),
    );
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["config", "user.email", "t@x"])
            .status(),
    );
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["config", "user.name", "t"])
            .status(),
    );
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["commit", "--allow-empty", "-m", "init", "-q"])
            .status(),
    );
    dir.to_string_lossy().into_owned()
}

/// parse_worktrees returns the main worktree row for a fresh repo, with
/// the branch name stripped of its refs/heads/ prefix, the head sha
/// shortened to seven characters, and is_current set when the working dir
/// matches (git canonicalizes the path, so the compare is canonical).
#[test]
fn test_parse_returns_main_row() {
    let repo = make_repo(2);
    let entries = parse_worktrees(Some(&repo), &repo);
    assert!(!entries.is_empty(), "at least the main worktree");
    let main = &entries[0];
    assert_eq!(
        canonicalize(&main.path).ok(),
        canonicalize(&repo).ok(),
        "main worktree path canonicalizes to the repo root"
    );
    assert_eq!(main.branch, "main", "branch stripped of refs/heads/");
    assert_eq!(main.head.len(), 7, "head sha shortened to 7 chars");
    assert!(main.is_current, "main flagged current when cwd matches");
    remove_dir_all(&repo).ok();
}

/// parse_worktrees marks a detached-HEAD worktree's branch as
/// "detached" (git porcelain emits the bare word, no parens). Pinned
/// because an earlier match on "(detached)" never hit, leaving the
/// branch empty and the pane rendering an empty bracket.
#[test]
fn test_parse_marks_detached_branch() {
    let repo = make_repo(3);
    let wt = temp_dir().join(format!("houyi-wt-detached-{}-3", id()));
    drop(remove_dir_all(&wt));
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "--detach", &wt.to_string_lossy(), "-q"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        "git worktree add --detach"
    );
    let entries = parse_worktrees(Some(&repo), &repo);
    let wt_canon = canonicalize(&wt).ok();
    let detached = entries
        .iter()
        .find(|e| canonicalize(&e.path).ok() == wt_canon);
    let detached = detached.expect("detached worktree listed");
    assert_eq!(detached.branch, "detached", "detached HEAD marked");
    remove_dir_all(&repo).ok();
    remove_dir_all(&wt).ok();
}

/// parse_worktrees returns an empty vec when no root is given, so the pane
/// renders the fallback message rather than indexing into nothing.
#[test]
fn test_parse_empty_no_root() {
    assert!(parse_worktrees(None, "/tmp").is_empty());
}
