//! Tests for this module.

use super::*;

#[test]
fn test_checkout_switch_not_consent() {
    assert!(!git_discard_form("checkout", &["main"]));
    assert!(!git_discard_form("checkout", &["feature-branch"]));
    assert!(!git_discard_form("checkout", &[]));
}

#[test]
fn test_checkout_path_force_discard() {
    assert!(git_discard_form("checkout", &["--", "file.rs"]));
    assert!(git_discard_form("checkout", &["."]));
    assert!(git_discard_form("checkout", &["-f", "main"]));
    assert!(git_discard_form("checkout", &["--force"]));
    assert!(git_discard_form("checkout", &["file1", "file2"]));
    assert!(git_discard_form("checkout", &["main", "file"]));
}

#[test]
fn test_restore_discard_rules() {
    assert!(git_discard_form("restore", &["file.rs"]));
    assert!(!git_discard_form("restore", &["--staged", "file.rs"]));
    assert!(git_discard_form(
        "restore",
        &["--staged", "--worktree", "file.rs"]
    ));
    assert!(git_discard_form("restore", &["-w", "file.rs"]));
}

#[test]
fn test_clean_needs_force_flag() {
    assert!(git_discard_form("clean", &["-f"]));
    assert!(git_discard_form("clean", &["-fd"]));
    assert!(git_discard_form("clean", &["-df"]));
    assert!(git_discard_form("clean", &["--force"]));
    assert!(!git_discard_form("clean", &["-x"]));
    assert!(!git_discard_form("clean", &[]));
}

#[test]
fn test_stash_drop_clear_pop() {
    assert!(git_discard_form("stash", &["drop"]));
    assert!(git_discard_form("stash", &["drop", "stash@{0}"]));
    assert!(git_discard_form("stash", &["clear"]));
    assert!(git_discard_form("stash", &["pop"]));
    assert!(!git_discard_form("stash", &[]));
    assert!(!git_discard_form("stash", &["list"]));
    assert!(!git_discard_form("stash", &["apply"]));
}

#[test]
fn test_branch_delete_discard() {
    assert!(git_discard_form("branch", &["-d", "feature"]));
    assert!(git_discard_form("branch", &["--delete", "feature"]));
    assert!(!git_discard_form("branch", &["feature"]));
    assert!(!git_discard_form("branch", &["-a"]));
}

#[test]
fn test_push_force_discard() {
    assert!(git_discard_form("push", &["--force"]));
    assert!(git_discard_form("push", &["--force-with-lease"]));
    assert!(git_discard_form("push", &["-f", "origin", "main"]));
    // = form not eaten by an env filter (the global-option skip handles it).
    assert!(git_discard_form(
        "push",
        &["--force-with-lease=origin/main"]
    ));
    assert!(!git_discard_form("push", &["origin", "main"]));
}

#[test]
fn test_non_discard_subcommands() {
    assert!(!git_discard_form("add", &["file"]));
    assert!(!git_discard_form("commit", &["-m", "x"]));
    assert!(!git_discard_form("status", &[]));
    assert!(!git_discard_form("log", &[]));
}

/// Aggressive snapshot: a single-path checkout DOES trigger a snapshot
/// (the static matcher cannot tell a branch from a file path). A bare
/// branch switch also snapshots (cheap, harmless) — the asymmetry vs the
/// conservative consent matcher is by design.
#[test]
fn test_snapshot_path_checkout_triggers() {
    assert!(git_snapshot_form("checkout", &["src/main.rs"]));
    assert!(git_snapshot_form("checkout", &["main"]));
    assert!(git_snapshot_form("checkout", &["--", "file.rs"]));
    assert!(git_snapshot_form("restore", &["file.rs"]));
    assert!(git_snapshot_form("stash", &[])); // bare stash saves + removes
    assert!(git_snapshot_form("stash", &["drop"]));
    assert!(git_snapshot_form("clean", &["-fd"]));
    assert!(git_snapshot_form("branch", &["-d", "x"]));
    assert!(git_snapshot_form("push", &["--force"]));
    // bare checkout (no args) + non-touching ops do NOT snapshot.
    assert!(!git_snapshot_form("checkout", &[]));
    assert!(!git_snapshot_form("status", &[]));
    assert!(!git_snapshot_form("log", &[]));
    assert!(!git_snapshot_form("stash", &["list"]));
}

/// The global-option + subshell + env-assignment bypasses (review cases A,
/// C, + the --force-with-lease= nit). Each of these used to slip past the
/// matcher entirely.
#[test]
fn test_snapshot_bypasses_closed() {
    // git -C <dir> checkout . — was bypassed (sub read as "-c").
    assert!(command_triggers_git_snapshot("git -C /path checkout ."));
    // git -c user.name=x checkout . — was bypassed (-c value eaten).
    assert!(command_triggers_git_snapshot(
        "git -c user.name=x checkout ."
    ));
    // git --work-tree /x checkout . — was bypassed.
    assert!(command_triggers_git_snapshot(
        "git --work-tree /x checkout ."
    ));
    // git --git-dir=/x checkout . — = form.
    assert!(command_triggers_git_snapshot("git --git-dir=/x checkout ."));
    // single-path checkout — the original data-loss path, now snapshots.
    assert!(command_triggers_git_snapshot("git checkout src/main.rs"));
    // subshell-wrapped.
    assert!(command_triggers_git_snapshot("(git checkout .)"));
    assert!(command_triggers_git_snapshot("{ git checkout . ; }"));
    // env-assignment prefix.
    assert!(command_triggers_git_snapshot("FOO=bar git checkout ."));
    // --force-with-lease= not eaten by an = filter.
    assert!(command_triggers_git_snapshot(
        "git push --force-with-lease=origin/main"
    ));
    // compound + interpreter-wrapped.
    assert!(command_triggers_git_snapshot(
        "git checkout . && git status"
    ));
    assert!(command_triggers_git_snapshot("bash -c \"git checkout .\""));
    assert!(command_triggers_git_snapshot("sh -c 'git clean -fd'"));
    // non-git + bare branch switch (aggressive: snapshots anyway, harmless).
    assert!(!command_triggers_git_snapshot("ls -la"));
    assert!(!command_triggers_git_snapshot("rm file"));
    assert!(!command_triggers_git_snapshot("git status"));
    assert!(!command_triggers_git_snapshot("git log"));
}

/// Shared drift vector: the commands where the conservative consent
/// matcher + the aggressive snapshot matcher MUST agree (clear-cut
/// discards). The permission crate holds an identical vector asserting
/// its conservative copy — keep both in sync. Asymmetric cases (single-
/// path checkout, bare stash) live in their own crate-specific tests.
const DRIFT_VECTOR: &[&str] = &[
    "git checkout .",
    "git checkout -- file.rs",
    "git checkout -f main",
    "git checkout --force",
    "git restore file.rs",
    "git restore -w file.rs",
    "git clean -fd",
    "git clean --force",
    "git stash drop",
    "git stash clear",
    "git stash pop",
    "git branch -D feature",
    "git branch -d feature",
    "git push --force origin main",
    "git push --force-with-lease",
    "git checkout main && git status", // switch segment not a discard
    "git status",
    "git log --oneline",
    "git stash list",
    "git stash apply",
];

#[test]
fn test_drift_vector_conservative_matches() {
    // The conservative git_discard_form (matches the permission crate's
    // git_discard_consent_word) on the shared DRIFT_VECTOR. If this
    // drifts from the permission side, the other crate's drift test
    // fails too — see permission/src/git_discard.rs DRIFT_VECTOR.
    let expected = [
        true, true, true, true, true, true, true, true, true, true, true, true, true, true, true,
        false, false, false, false, false,
    ];
    assert_eq!(DRIFT_VECTOR.len(), expected.len());
    for (cmd, exp) in DRIFT_VECTOR.iter().zip(expected.iter()) {
        let lower = cmd.to_ascii_lowercase();
        // Split on compound separators like the real matchers, take the
        // first git segment.
        let (sub, args) = lower
            .split(['&', ';', '|', '\n'])
            .find_map(|seg| git_sub_and_args(&seg.split_whitespace().collect::<Vec<_>>()))
            .unwrap_or_else(|| panic!("no git sub in {cmd}"));
        assert_eq!(
            git_discard_form(sub, &args),
            *exp,
            "conservative drift on {cmd}"
        );
    }
}
