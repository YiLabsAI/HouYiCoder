//! What a pane-driven removal does with a tree this session is working in, and
//! with one it is not.

use std::fs::remove_dir_all;
use std::path::Path;
use std::path::PathBuf;
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
/// of naming another process. The session holds that tree's lease, so a
/// removal that went as far as taking the lease would fail as if someone else
/// held it; the controller compares the session's own worktree name first.
/// Nothing is taken apart.
#[tokio::test]
async fn test_removal_refuses_active_tree() {
    let repo = make_repo(41);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let entered = controller
        .enter(Some("pane-active".into()))
        .await
        .expect("enter");
    let preview = controller.preview_remove("pane-active").expect("preview");
    assert!(preview.leased, "the entered tree reads as in use");

    let error = controller
        .remove_previewed("pane-active", &preview.token, true)
        .expect_err("the session works in it");

    match &error {
        WorktreeError::Leased { slug, reason } => {
            assert_eq!(slug, "pane-active");
            assert_eq!(reason, "this session is working in it");
        }
        other => panic!("expected this session's own hold to be named, got {other:?}"),
    }
    assert!(
        Path::new(&entered.worktree_path).exists(),
        "the tree survives"
    );
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
    let preview = controller.preview_remove("pane-released").expect("preview");
    assert!(!preview.leased, "the lease went with the session");

    controller
        .remove_previewed("pane-released", &preview.token, true)
        .expect("the tree is removed");

    assert!(
        !Path::new(&entered.worktree_path).exists(),
        "the tree is gone"
    );
    drop(remove_dir_all(&repo));
}

/// A removal driven from the pane tells automations through the same event the
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
    let preview = controller.preview_remove("pane-fire").expect("preview");
    controller
        .remove_previewed("pane-fire", &preview.token, true)
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
