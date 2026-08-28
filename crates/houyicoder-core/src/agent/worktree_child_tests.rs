//! Per-child worktree + fence tests. Reuses the sibling test module's
//! harness (make_repo, wired, wired_err); the helpers are pub(crate).

use houyicoder_api::hook_fire::HookFire;
use houyicoder_context::{HookEventKind, HookFirePayload, SessionId, TurnEventKind};
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;
use std::sync::{Arc, Mutex};

use crate::agent::ToolRegistry;
use crate::agent::multi_agent::registry::IsolationMode;
use crate::agent::multi_agent::spawn::{SpawnError, SpawnRequest, TriggerSource, spawn_child};
use crate::agent::runner_config::RunnerConfig;
use crate::provider::test_support::FakeProvider;

use super::ChildCleanup;
use super::ExitAction;
use super::tests::{make_repo, wired, wired_err};

/// enter_for_child creates a worktree + returns a guard, without entering a
/// user worktree session or repointing the parent cwd. The parent stays
/// out of a session and on its original cwd -- the per-child isolation
/// contract that distinguishes it from the user-facing enter.
#[tokio::test]
async fn test_enter_child_leaves_parent() {
    let repo = make_repo(10);
    let (controller, _store, cwd) = wired(&repo, 0);
    let cw = controller
        .enter_for_child("agent-deadbeef".into())
        .await
        .expect("enter_for_child");
    assert!(cw.worktree_path.exists(), "worktree dir created");
    assert_ne!(
        cw.worktree_path, repo,
        "child worktree is separate from repo"
    );
    assert!(!cw.worktree_branch.is_empty(), "branch name present");
    assert!(
        !controller.current(),
        "parent not in a user worktree session"
    );
    assert_eq!(*cwd.read().unwrap(), repo, "parent cwd unchanged");
    std::fs::remove_dir_all(&repo).ok();
}

/// enter_for_child refuses while a sandbox exec is in flight (an in-flight
/// exec keeps its spawn-time profile and could escape the narrow). The
/// fail-closed guard the user-facing enter also enforces.
#[tokio::test]
async fn test_enter_child_refused_inflight() {
    let repo = make_repo(11);
    let (controller, _store, _cwd) = wired(&repo, 1);
    match controller.enter_for_child("agent-inflight".into()).await {
        Ok(_) => panic!("in-flight must refuse"),
        Err(e) => assert!(e.to_string().contains("in flight"), "{e}"),
    }
    std::fs::remove_dir_all(&repo).ok();
}

/// spawn_child with Worktree isolation creates a child + a per-child
/// worktree, and the SubagentSpawn boundary records isolation="worktree"
/// (not the hardcoded "none" from the no-isolation path).
#[tokio::test]
async fn test_spawn_child_worktree_isolation() {
    let repo = make_repo(12);
    let (controller, store, _cwd) = wired(&repo, 0);
    let parent_sid = SessionId::new();
    let provider: std::sync::Arc<dyn houyicoder_api::provider::ModelProvider> =
        std::sync::Arc::new(FakeProvider::text("ok"));
    let req = SpawnRequest {
        parent_sid,
        parent_store: store.clone(),
        provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig {
            model: "parent-model".into(),
            ..RunnerConfig::default()
        },
        subagent_type: "explore".to_string(),
        prompt: "find auth".to_string(),
        prompt_summary: "find auth".to_string(),
        trigger: TriggerSource::ModelTool {
            tool_call_id: "wt-call".to_string(),
        },
        depth: 0,
        isolation: IsolationMode::Worktree,
        worktree_controller: Some(controller.clone()),
        run_in_background: false,
        parent_cancel: None,
        bus: None,
    };
    let handle = spawn_child(req).await.expect("spawn");
    assert!(handle.worktree.is_some(), "child carries a worktree guard");
    let events = store.trajectory_snapshot(parent_sid);
    let iso = events
        .iter()
        .find_map(|e| match &e.kind {
            TurnEventKind::SubagentSpawn { isolation, .. } => Some(isolation.clone()),
            _ => None,
        })
        .expect("spawn boundary present");
    assert_eq!(iso, "worktree", "boundary records the worktree isolation");
    drop(handle);
    std::fs::remove_dir_all(&repo).ok();
}

/// A worktree fence failure rejects the spawn (fail-closed: no degradation
/// to no isolation) and writes no boundary -- no phantom spawn for resume
/// to reconcile against a child that never ran.
#[tokio::test]
async fn test_spawn_child_fence_fail() {
    let repo = make_repo(13);
    let controller = wired_err(&repo);
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: std::sync::Arc<dyn houyicoder_api::provider::ModelProvider> =
        std::sync::Arc::new(FakeProvider::text("ok"));
    let req = SpawnRequest {
        parent_sid,
        parent_store: store.clone(),
        provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig {
            model: "parent-model".into(),
            ..RunnerConfig::default()
        },
        subagent_type: "explore".to_string(),
        prompt: "find auth".to_string(),
        prompt_summary: "find auth".to_string(),
        trigger: TriggerSource::ModelTool {
            tool_call_id: "wt-call".to_string(),
        },
        depth: 0,
        isolation: IsolationMode::Worktree,
        worktree_controller: Some(controller),
        run_in_background: false,
        parent_cancel: None,
        bus: None,
    };
    match spawn_child(req).await {
        Ok(_) => panic!("fence failure must reject, not spawn"),
        Err(e) => assert_eq!(e, SpawnError::WorktreeFenceNarrowFail),
    }
    assert!(
        store.trajectory_snapshot(parent_sid).is_empty(),
        "a rejected spawn writes no boundary"
    );
    std::fs::remove_dir_all(&repo).ok();
}

/// cleanup_child on a clean worktree (no uncommitted files, no new commits)
/// restores the fence + removes the worktree dir -- terminal-state
/// auto-cleanup so finished children leave no stale trees behind.
#[tokio::test]
async fn test_cleanup_clean_worktree_removed() {
    let repo = make_repo(20);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let cw = controller
        .enter_for_child("agent-clean".into())
        .await
        .expect("enter");
    let path = cw.worktree_path.clone();
    let outcome = controller.cleanup_child(cw).await.expect("cleanup");
    match outcome {
        ChildCleanup::Removed { worktree_path } => assert_eq!(worktree_path, path),
        ChildCleanup::Kept { .. } => panic!("clean worktree must be removed, not kept"),
    }
    assert!(!path.exists(), "clean worktree dir removed");
    std::fs::remove_dir_all(&repo).ok();
}

/// cleanup_child on a dirty worktree (uncommitted changes) restores the
/// fence + keeps the worktree, reporting path + branch so the caller can
/// continue on the branch -- never silently destroy work the child produced.
#[tokio::test]
async fn test_cleanup_dirty_worktree_kept() {
    let repo = make_repo(21);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let cw = controller
        .enter_for_child("agent-dirty".into())
        .await
        .expect("enter");
    std::fs::write(cw.worktree_path.join("uncommitted.txt"), "dirty").expect("write");
    let path = cw.worktree_path.clone();
    let outcome = controller.cleanup_child(cw).await.expect("cleanup");
    match outcome {
        ChildCleanup::Kept {
            worktree_path,
            worktree_branch,
        } => {
            assert_eq!(worktree_path, path);
            assert!(!worktree_branch.is_empty(), "branch reported back");
        }
        ChildCleanup::Removed { .. } => panic!("dirty worktree must be kept, not removed"),
    }
    assert!(path.exists(), "dirty worktree dir preserved");
    std::fs::remove_dir_all(&repo).ok();
}

/// A HookFire that records the event kinds it was called with, for asserting
/// the controller fires at the per-child enter and cleanup boundaries.
struct RecordingHookFire {
    events: Arc<Mutex<Vec<HookEventKind>>>,
}
impl HookFire for RecordingHookFire {
    fn fire(
        &self,
        event: HookEventKind,
        _payload: HookFirePayload,
    ) -> houyicoder_async::PFut<'_, ()> {
        self.events.lock().expect("recorder lock").push(event);
        Box::pin(async {})
    }
}

/// enter_for_child fires WorktreeCreate at the per-child spawn boundary, so
/// the controller covers the spawn path (not only the user-facing enter tool).
#[tokio::test]
async fn test_enter_fires_worktree_create() {
    let repo = make_repo(30);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let events = Arc::new(Mutex::new(Vec::new()));
    controller.set_hook_fire(Some(Arc::new(RecordingHookFire {
        events: events.clone(),
    }) as Arc<dyn HookFire>));
    let _cw = controller
        .enter_for_child("agent-fire".into())
        .await
        .expect("enter");
    let fired = events.lock().expect("events lock").clone();
    assert!(
        fired.contains(&HookEventKind::WorktreeCreate),
        "enter_for_child fires WorktreeCreate: {fired:?}"
    );
    std::fs::remove_dir_all(&repo).ok();
}

/// cleanup_child on a removed (clean) worktree fires WorktreeRemove at the
/// boundary, pairing with the WorktreeCreate the enter fired.
#[tokio::test]
async fn test_cleanup_removed_fires_remove() {
    let repo = make_repo(31);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let events = Arc::new(Mutex::new(Vec::new()));
    controller.set_hook_fire(Some(Arc::new(RecordingHookFire {
        events: events.clone(),
    }) as Arc<dyn HookFire>));
    let cw = controller
        .enter_for_child("agent-clean".into())
        .await
        .expect("enter");
    let outcome = controller.cleanup_child(cw).await.expect("cleanup");
    assert!(
        matches!(outcome, ChildCleanup::Removed { .. }),
        "clean worktree removed"
    );
    let fired = events.lock().expect("events lock").clone();
    assert!(
        fired.contains(&HookEventKind::WorktreeCreate)
            && fired.contains(&HookEventKind::WorktreeRemove),
        "enter + cleanup fire Create + Remove: {fired:?}"
    );
    std::fs::remove_dir_all(&repo).ok();
}

/// The user-facing enter + exit(Remove) tool path fires WorktreeCreate +
/// WorktreeRemove at the controller boundary, so the tool path and the spawn
/// path share one fire point.
#[tokio::test]
async fn test_toolpath_fires_create_remove() {
    let repo = make_repo(40);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let events = Arc::new(Mutex::new(Vec::new()));
    controller.set_hook_fire(Some(Arc::new(RecordingHookFire {
        events: events.clone(),
    }) as Arc<dyn HookFire>));
    controller
        .enter(Some("user-wt".into()))
        .await
        .expect("enter");
    controller
        .exit(ExitAction::Remove, true)
        .await
        .expect("exit remove");
    let fired = events.lock().expect("events lock").clone();
    assert!(
        fired.contains(&HookEventKind::WorktreeCreate),
        "enter fires WorktreeCreate: {fired:?}"
    );
    assert!(
        fired.contains(&HookEventKind::WorktreeRemove),
        "exit Remove fires WorktreeRemove: {fired:?}"
    );
    std::fs::remove_dir_all(&repo).ok();
}
