//! The exit tool's approval gate and the by-name managed removal.

use std::fs::remove_dir_all;
use std::fs::write;
use std::path::PathBuf;
use std::process::Command;

use houyicoder_api::tool::Tool;
use houyicoder_api::tool::ToolCtx;
use houyicoder_context::SessionId;
use serde_json::json;

use super::ExitWorktreeTool;
use crate::agent::worktree::controller::tests::make_repo;
use crate::agent::worktree::controller::tests::wired;
use crate::agent::worktree_controller::ExitAction;

/// A remove with no tree behind it removes nothing, so asking the user to
/// confirm it asks them to approve an act that cannot happen. A resumed
/// session holds no record of entering a tree, and the card that used to
/// appear for it failed the moment it was accepted.
#[test]
fn test_remove_untracked_asks_nothing() {
    let repo = make_repo(91);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let tool = ExitWorktreeTool::new(controller);
    assert!(
        !tool.requires_approval_for(&json!({"action": "remove"})),
        "no active session and no name means no removable tree"
    );
    drop(remove_dir_all(&repo));
}

/// Keep never asks: it leaves everything on disk, and the exit card is for a
/// removal the user has to accept.
#[test]
fn test_keep_needs_no_approval() {
    let repo = make_repo(92);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let tool = ExitWorktreeTool::new(controller);
    assert!(!tool.requires_approval_for(&json!({"action": "keep"})));
    drop(remove_dir_all(&repo));
}

/// A removal that names a tree and discards its work is a real removal, so the
/// user is asked. A removal that keeps the work runs without a card. The name
/// is what tells the gate there is something to take apart when the session
/// itself is not inside one.
#[test]
fn test_named_remove_needs_approval() {
    let repo = make_repo(93);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let tool = ExitWorktreeTool::new(controller);
    assert!(tool.requires_approval_for(&json!({
        "action": "remove",
        "name": "s36-ux-fix",
        "discard_changes": true
    })));
    assert!(!tool.requires_approval_for(&json!({
        "action": "remove",
        "name": "s36-ux-fix"
    })));
    drop(remove_dir_all(&repo));
}

/// A managed tree the session is not inside is taken apart by the host: the
/// directory and its branch both go, so a caller never has to reach for a
/// shell removal. A shell removal cannot finish this job, because the fence
/// refuses any tree carrying a protected path and leaves the registration gone
/// with half a directory on disk behind it.
#[tokio::test]
async fn test_named_remove_clears_tree() {
    let repo = make_repo(94);
    let (controller, _store, _cwd) = wired(&repo, 0);
    // A tree this product created, so the managed path owns its record.
    let created = controller
        .enter(Some("left-behind".into()))
        .await
        .expect("enter");
    controller
        .exit(ExitAction::Keep, false)
        .await
        .expect("keep");
    let tree = PathBuf::from(&created.worktree_path);
    assert!(tree.is_dir(), "the kept tree is on disk");

    let tool = ExitWorktreeTool::new(controller);
    let out = tool
        .execute(
            ToolCtx::new("call").with_session(SessionId::new()),
            json!({"action": "remove", "name": "left-behind", "discard_changes": true}),
        )
        .await
        .expect("the host removes the named tree");
    assert_eq!(out["action"], "remove");
    assert!(!tree.exists(), "the directory is gone: {}", tree.display());
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding reading a branch list, not an engine spawn"
    )]
    let branches = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["branch", "--list", "worktree-left-behind"])
        .output()
        .expect("list branches");
    assert!(
        String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
        "the branch went with the tree"
    );
    drop(remove_dir_all(&repo));
}

/// A slug says which tree, not what may be destroyed. A named removal without
/// an explicit acceptance of loss must leave uncommitted work where it is:
/// the tool contract says a remove with such work refuses, and a caller that
/// never said "discard" has not agreed to lose it.
#[tokio::test]
async fn test_named_remove_keeps_work() {
    let repo = make_repo(95);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let created = controller
        .enter(Some("with-work".into()))
        .await
        .expect("enter");
    controller
        .exit(ExitAction::Keep, false)
        .await
        .expect("keep");
    let tree = PathBuf::from(&created.worktree_path);
    write(tree.join("draft.txt"), "unsaved thought\n").expect("write a change");

    let tool = ExitWorktreeTool::new(controller);
    let refused = tool
        .execute(
            ToolCtx::new("call").with_session(SessionId::new()),
            json!({"action": "remove", "name": "with-work"}),
        )
        .await;
    assert!(
        refused.is_err(),
        "a dirty tree is not removed without an explicit discard"
    );
    assert!(
        tree.join("draft.txt").exists(),
        "the uncommitted file survives the refusal"
    );
    drop(remove_dir_all(&repo));
}
