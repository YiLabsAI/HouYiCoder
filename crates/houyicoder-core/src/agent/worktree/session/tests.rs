#![expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]

use super::*;
use std::env::temp_dir;
use std::fs;
use std::process::id;

/// A throwaway git repo with one commit, so a remove test runs its body
/// deterministically instead of skipping outside a live work tree.
fn make_repo(tag: &str) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-wts-{tag}-{}", id()));
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(&dir).expect("repo dir");
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "-q"])
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
    dir
}

#[test]
fn test_validates_legal_slugs() {
    assert!(validate_worktree_slug("feat-x").is_ok());
    assert!(validate_worktree_slug("user/feature").is_ok());
    assert!(validate_worktree_slug("a.b_c-d").is_ok());
}

#[test]
fn test_rejects_bad_slugs() {
    assert!(validate_worktree_slug("").is_err(), "empty");
    assert!(validate_worktree_slug(".").is_err(), "dot");
    assert!(validate_worktree_slug("..").is_err(), "dotdot");
    assert!(validate_worktree_slug("a/../b").is_err(), "traversal");
    assert!(validate_worktree_slug("/leading").is_err(), "leading slash");
    assert!(
        validate_worktree_slug("trailing/").is_err(),
        "trailing slash"
    );
    assert!(validate_worktree_slug("a//b").is_err(), "empty segment");
    assert!(validate_worktree_slug("bad space").is_err(), "space");
    assert!(validate_worktree_slug("bad:colon").is_err(), "colon");
    assert!(validate_worktree_slug(&"x".repeat(65)).is_err(), "too long");
}

#[test]
fn test_flatten_replaces_slash() {
    assert_eq!(flatten_slug("user/feature"), "user+feature");
    assert_eq!(flatten_slug("plain"), "plain");
    assert_eq!(flatten_slug("a/b/c"), "a+b+c");
}

#[test]
fn test_branch_name_uses_slug() {
    assert_eq!(
        worktree_branch_name("user/feature"),
        "worktree-user+feature"
    );
    let repo = Path::new("/repo");
    assert_eq!(
        worktree_path_for(repo, "user/feature"),
        Path::new("/repo/.houyicoder/worktrees/user+feature")
    );
}

/// Smoke test against a real throwaway worktree of the host repo: create
/// then content visible then resume same path then cleanup. Skipped when
/// not run from a git work tree.
#[test]
fn test_get_create_round_trips() {
    let cwd = current_dir().expect("cwd");
    if !cwd.join(".git").exists() {
        return;
    }
    let slug = "smoke-round-trip";
    drop(cleanup_worktree(&cwd, slug));
    let r = get_or_create_worktree(&cwd, slug).expect("create");
    assert!(!r.existed, "first call creates");
    assert!(
        r.worktree_path.join("Cargo.toml").exists(),
        "worktree sees repo content"
    );
    assert_eq!(r.worktree_branch, "worktree-smoke-round-trip");
    let r2 = get_or_create_worktree(&cwd, slug).expect("resume");
    assert!(r2.existed, "second call resumes");
    assert_eq!(r.worktree_path, r2.worktree_path);
    drop(cleanup_worktree(&cwd, slug));
    assert!(
        !r.worktree_path.exists(),
        "worktree dir removed after cleanup"
    );
}

/// A pre-existing same-named branch makes the -b add collide (the
/// worktree dir does not yet exist, so the fast-resume path is skipped).
#[test]
fn test_collision_on_existing_branch() {
    let cwd = current_dir().expect("cwd");
    if !cwd.join(".git").exists() {
        return;
    }
    let slug = "smoke-collision";
    drop(cleanup_worktree(&cwd, slug));
    let branch = worktree_branch_name(slug);
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&cwd)
            .args(["branch", &branch])
            .status(),
    );
    let err = get_or_create_worktree(&cwd, slug).expect_err("collision");
    assert!(
        matches!(err, WorktreeError::Collision { .. }),
        "expected Collision, got {err:?}"
    );
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&cwd)
            .args(["branch", "-D", &branch])
            .status(),
    );
}

/// Fail-closed: count_worktree_changes returns None when there is no
/// baseline commit (cannot prove clean, assume unsafe).
#[test]
fn test_count_changes_without_baseline() {
    assert!(count_worktree_changes(Path::new("/nonexistent"), None).is_none());
}

/// The conservative remove deletes both the tree dir and its branch after
/// a clean create: no --force needed, and branch -d accepts a branch that
/// has no unmerged work.
#[test]
fn test_remove_clean_removes_tree() {
    let repo = make_repo("removes-tree");
    let slug = "clean-remove";
    let created = get_or_create_worktree(&repo, slug).expect("create");
    assert!(created.worktree_path.exists(), "tree created");
    remove_clean_worktree(&repo, slug).expect("clean remove");
    assert!(!created.worktree_path.exists(), "tree dir removed");
    let branch = worktree_branch_name(slug);
    assert!(
        run_git(&repo, &["rev-parse", "--verify", &branch]).is_err(),
        "branch deleted with the tree"
    );
    drop(fs::remove_dir_all(&repo));
}

/// A conservative remove of a tree that does not exist fails on its first
/// git step, so the branch delete is never reached.
#[test]
fn test_remove_clean_fails_missing() {
    let repo = make_repo("fails-missing");
    let err = remove_clean_worktree(&repo, "no-such-clean-tree").expect_err("missing tree fails");
    assert!(
        matches!(err, WorktreeError::Git { .. }),
        "git failure: {err:?}"
    );
    drop(fs::remove_dir_all(&repo));
}

fn cleanup_worktree(repo_root: &Path, slug: &str) -> Result<(), WorktreeError> {
    let path = worktree_path_for(repo_root, slug);
    let branch = worktree_branch_name(slug);
    drop(run_git(
        repo_root,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    ));
    drop(run_git(repo_root, &["branch", "-D", &branch]));
    Ok(())
}
