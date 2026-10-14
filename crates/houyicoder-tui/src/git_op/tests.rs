use super::*;

#[test]
fn test_detect_commit_sha() {
    let op = detect_git_operation("git commit -m x", "[main abc1234] x\n 1 file changed");
    assert_eq!(op.commit.as_ref().map(|(s, _)| s.as_str()), Some("abc123"));
}

#[test]
fn test_detect_commit_amend() {
    let op = detect_git_operation("git commit --amend", "[main def5678] x");
    assert_eq!(
        op.commit.as_ref().map(|(_, k)| *k),
        Some(CommitKind::Amended)
    );
}

#[test]
fn test_detect_cherry_pick() {
    let op = detect_git_operation("git cherry-pick abc", "[feature 111aaa] x");
    assert_eq!(
        op.commit.as_ref().map(|(_, k)| *k),
        Some(CommitKind::CherryPicked)
    );
}

#[test]
fn test_detect_push_branch() {
    let op = detect_git_operation(
        "git push",
        "To github.com:o/r.git\n   abc..def  main -> main",
    );
    assert_eq!(op.push.as_deref(), Some("main"));
}

#[test]
fn test_detect_pr_create() {
    let op = detect_git_operation("gh pr create", "https://github.com/owner/repo/pull/42");
    let pr = op.pr.expect("pr");
    assert_eq!(pr.number, 42);
    assert_eq!(pr.action, PrAction::Created);
}

#[test]
fn test_detect_pr_merge_text() {
    let op = detect_git_operation("gh pr merge 42", "Merged pull request owner/repo#42");
    let pr = op.pr.expect("pr");
    assert_eq!(pr.number, 42);
    assert_eq!(pr.action, PrAction::Merged);
}

#[test]
fn test_detect_merge_ref() {
    let op = detect_git_operation("git merge feature", "Fast-forward");
    assert_eq!(op.branch.as_ref().map(|(r, _)| r.as_str()), Some("feature"));
    assert_eq!(
        op.branch.as_ref().map(|(_, a)| *a),
        Some(BranchAction::Merged)
    );
}

#[test]
fn test_detect_rebase_ref() {
    let op = detect_git_operation("git rebase main", "Successfully rebased");
    assert_eq!(op.branch.as_ref().map(|(r, _)| r.as_str()), Some("main"));
    assert_eq!(
        op.branch.as_ref().map(|(_, a)| *a),
        Some(BranchAction::Rebased)
    );
}

#[test]
fn test_is_git_op_classifies() {
    assert!(is_git_op_command("git commit -m x"));
    assert!(is_git_op_command("git -c k=v push"));
    assert!(is_git_op_command("gh pr create"));
    assert!(!is_git_op_command("git log"));
    assert!(!is_git_op_command("ls -la"));
}

#[test]
fn test_log_no_false_commit() {
    // A SHA in git log output does not make a non-commit command a commit.
    let op = detect_git_operation("git log", "[main abc1234] old commit");
    assert!(op.commit.is_none());
}
