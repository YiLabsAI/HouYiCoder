//! Per-child worktree + fence tests. Reuses the sibling test module's
//! harness (make_repo, wired, wired_err); the helpers are pub(crate).

use houyicoder_context::{SessionId, TurnEventKind};
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;
use std::sync::Arc;

use crate::agent::ToolRegistry;
use crate::agent::multi_agent::registry::IsolationMode;
use crate::agent::multi_agent::spawn::{SpawnError, SpawnRequest, spawn_child};
use crate::agent::runner_config::RunnerConfig;
use crate::provider::test_support::FakeProvider;

use super::tests::{make_repo, wired, wired_err};

/// enter_for_child creates a worktree + returns a guard, without entering a
/// user worktree session or repointing the parent cwd. The parent stays
/// out of a session and on its original cwd -- the per-child isolation
/// contract that distinguishes it from the user-facing enter.
#[test]
fn test_enter_child_leaves_parent() {
    let repo = make_repo(10);
    let (controller, _store, cwd) = wired(&repo, 0);
    let cw = controller
        .enter_for_child("agent-deadbeef".into())
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
#[test]
fn test_enter_child_refused_inflight() {
    let repo = make_repo(11);
    let (controller, _store, _cwd) = wired(&repo, 1);
    match controller.enter_for_child("agent-inflight".into()) {
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
        depth: 0,
        isolation: IsolationMode::Worktree,
        worktree_controller: Some(controller.clone()),
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
        depth: 0,
        isolation: IsolationMode::Worktree,
        worktree_controller: Some(controller),
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
