//! What a pane-driven removal does with a tree this session is working in, and
//! with one it is not.

use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;

use houyicoder_api::hook_fire::HookFire;
use houyicoder_context::HookEventKind;

use crate::agent::worktree::session::WorktreeError;

use super::super::ExitAction;
use super::child::RecordingHookFire;
use super::make_repo;
use super::wired;

/// The tree this session works in is refused, and the answer says so instead
/// of naming another process. The session holds that tree's lease, so a verb
/// that went as far as taking the lease would fail as if someone else held it;
/// the controller compares the session's own directory first. Nothing is taken
/// apart.
#[tokio::test]
async fn test_removal_refuses_active_tree() {
    let repo = make_repo(41);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let entered = controller
        .enter(Some("pane-active".into()))
        .await
        .expect("enter");
    let tree = PathBuf::from(&entered.worktree_path);

    let error = controller
        .preview_at(&tree)
        .map(|_| ())
        .expect_err("the session works in it");
    match &error {
        WorktreeError::ActiveTree { path } => assert_eq!(path, &tree),
        other => panic!("expected this session's own tree to be named, got {other:?}"),
    }
    let removal = controller
        .remove_at_previewed(&tree, "unused", true)
        .expect_err("the session works in it");
    assert!(
        matches!(removal, WorktreeError::ActiveTree { .. }),
        "the removal is refused before the token is looked at: {removal}"
    );
    assert!(tree.exists(), "the tree survives");
    controller
        .exit(ExitAction::Keep, false)
        .await
        .expect("exit keep");
    drop(remove_dir_all(&repo));
}

/// Once the session leaves without taking the tree, the same removal runs: the
/// refusal above names the session's hold, not the worktree itself.
#[tokio::test]
async fn test_removal_runs_after_exit() {
    let repo = make_repo(42);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let entered = controller
        .enter(Some("pane-released".into()))
        .await
        .expect("enter");
    controller
        .exit(ExitAction::Keep, false)
        .await
        .expect("exit keep");
    let tree = PathBuf::from(&entered.worktree_path);
    let (_target, preview) = controller.preview_at(&tree).expect("preview");
    assert!(!preview.leased, "the lease went with the session");

    controller
        .remove_at_previewed(&tree, &preview.token, true)
        .expect("the tree is removed");

    assert!(
        !Path::new(&entered.worktree_path).exists(),
        "the tree is gone"
    );
    drop(remove_dir_all(&repo));
}

/// A tree another tool made: a linked worktree outside the directory this
/// product creates its own in, so it carries no creation record.
fn foreign_tree(repo: &Path, name: &str) -> PathBuf {
    let path = repo.join(".claude").join("worktrees").join(name);
    create_dir_all(path.parent().expect("tree parent")).expect("tree parent");
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "-b", name, &path.to_string_lossy()])
        .status()
        .expect("run git");
    assert!(status.success(), "git worktree add failed");
    path
}

/// Whether the repository holds a branch by this name.
fn branch_exists(repo: &Path, name: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", &format!("refs/heads/{name}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run git")
        .success()
}

/// The repository's own working tree is refused by every verb the pane drives,
/// so no page offers to enter or remove it.
#[tokio::test]
async fn test_pane_refuses_main_checkout() {
    let repo = make_repo(44);
    let (controller, _store, _cwd) = wired(&repo, 0);

    let preview = controller.preview_at(&repo).map(|_| ());
    assert!(
        matches!(preview, Err(WorktreeError::MainCheckout { .. })),
        "the preview refuses the main tree: {preview:?}"
    );
    let removal = controller.remove_at_previewed(&repo, "unused", true);
    assert!(
        matches!(removal, Err(WorktreeError::MainCheckout { .. })),
        "the removal refuses the main tree: {removal:?}"
    );
    let enter = controller.enter_at_prepared(&repo);
    assert!(
        matches!(enter, Err(WorktreeError::MainCheckout { .. })),
        "the enter refuses the main tree: {enter:?}"
    );
    assert!(!controller.current(), "no session was installed");
    drop(remove_dir_all(&repo));
}

/// A tree this product did not create is entered as it stands and left with
/// its branch when the session ends: the session works in the directory Git
/// lists, and the removal takes the directory only.
#[tokio::test]
async fn test_adopt_foreign_tree() {
    let repo = make_repo(45);
    let tree = foreign_tree(&repo, "adopted");
    let (controller, _store, cwd) = wired(&repo, 0);

    let prepared = controller.enter_at_prepared(&tree).expect("enter the tree");
    let entered = controller
        .finish_enter(prepared)
        .await
        .expect("record the enter");

    assert_eq!(
        entered.worktree_branch, "adopted",
        "the session is on the branch the tree is checked out on"
    );
    assert_eq!(
        *cwd.read().expect("cwd lock"),
        dunce::canonicalize(&tree).expect("resolved tree"),
        "the session works in the tree it adopted"
    );

    controller
        .exit(ExitAction::Remove, true)
        .await
        .expect("the removal runs");

    assert!(!tree.exists(), "the directory is gone");
    assert!(
        branch_exists(&repo, "adopted"),
        "the branch belongs to whoever made the tree, so it stays"
    );
    assert!(!controller.current(), "the session ended");
    drop(remove_dir_all(&repo));
}
/// exit path fires. The removal changes no session state, so the event is the
/// whole notice they get; the caller fires it once the tree is gone.
#[tokio::test]
async fn test_pane_removal_fires_remove() {
    let repo = make_repo(43);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let entered = controller
        .enter(Some("pane-fire".into()))
        .await
        .expect("enter");
    controller
        .exit(ExitAction::Keep, false)
        .await
        .expect("exit keep");
    let events = Arc::new(Mutex::new(Vec::new()));
    controller.set_hook_fire(Some(Arc::new(RecordingHookFire {
        events: Arc::clone(&events),
    }) as Arc<dyn HookFire>));
    let tree = PathBuf::from(&entered.worktree_path);
    let (_target, preview) = controller.preview_at(&tree).expect("preview");
    controller
        .remove_at_previewed(&tree, &preview.token, true)
        .expect("the tree is removed");
    assert!(
        events.lock().expect("events lock").is_empty(),
        "the removal alone changes no session state, so it fires nothing"
    );

    controller
        .fire_removed(PathBuf::from(&entered.worktree_path))
        .await;

    let fired = events.lock().expect("events lock").clone();
    assert_eq!(
        fired,
        vec![HookEventKind::WorktreeRemove],
        "the pane removal fires the event once"
    );
    drop(remove_dir_all(&repo));
}
