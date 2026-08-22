//! TDD anchor for spawn_child: the minimal contract before implementation.

use super::{SpawnError, SpawnRequest, spawn_child};
use houyicoder_async::CancellationToken;
use houyicoder_context::{SessionId, TurnEventKind};
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;
use std::sync::Arc;

use crate::agent::ToolRegistry;
use crate::agent::multi_agent::registry::IsolationMode;
use crate::agent::runner_config::RunnerConfig;
use crate::provider::test_support::FakeProvider;

fn req_at_depth(
    parent_sid: SessionId,
    store: Arc<SessionStore>,
    provider: Arc<dyn houyicoder_api::provider::ModelProvider>,
    depth: u32,
) -> SpawnRequest {
    SpawnRequest {
        parent_sid,
        parent_store: store,
        provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig {
            model: "parent-model".into(),
            ..RunnerConfig::default()
        },
        subagent_type: "explore".to_string(),
        prompt: "find the auth module".to_string(),
        prompt_summary: "find the auth module".to_string(),
        depth,
        isolation: IsolationMode::None,
        worktree_controller: None,
        run_in_background: false,
        parent_cancel: None,
    }
}

/// spawn_child creates a child session id distinct from the parent,
/// records a SubagentSpawn durable boundary in the parent log, and
/// returns a ChildHandle carrying the child session id + cancel token.
#[tokio::test]
async fn test_spawn_creates_boundary() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::text("ok"));
    let req = req_at_depth(parent_sid, store.clone(), provider, 0);
    let handle = spawn_child(req).await.expect("spawn should succeed");
    assert_ne!(handle.session, parent_sid);

    // The parent log must carry a SubagentSpawn boundary whose
    // child_session_id matches the handle's session id. Resume and orphan
    // reconciliation pair spawn with return on this id; a mismatched or
    // missing id breaks the durable chain.
    let parent_events = store.trajectory_snapshot(parent_sid);
    let spawn = parent_events
        .iter()
        .find(|e| matches!(e.kind, TurnEventKind::SubagentSpawn { .. }))
        .expect("parent log must carry the spawn boundary");
    let recorded_child = match &spawn.kind {
        TurnEventKind::SubagentSpawn {
            child_session_id, ..
        } => child_session_id.clone(),
        _ => unreachable!("matched above"),
    };
    assert_eq!(
        recorded_child,
        handle.session.to_string(),
        "spawn boundary must record the child session id"
    );
}

/// The recursion guard rejects a spawn at the depth cap before any side
/// effect: no child session is created, no boundary is written to the
/// parent log. A reject that left a dangling boundary would break resume
/// (an unpaired spawn the parent never ran).
#[tokio::test]
async fn test_spawn_rejects_depth_cap() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::text("ok"));
    let req = req_at_depth(parent_sid, store.clone(), provider, 4);
    match spawn_child(req).await {
        Ok(_) => panic!("spawn at depth cap must reject, not succeed"),
        Err(e) => assert_eq!(e, SpawnError::SpawnRecursive),
    }
    let parent_events = store.trajectory_snapshot(parent_sid);
    assert!(
        parent_events.is_empty(),
        "a rejected spawn must write no boundary: {:?}",
        parent_events
    );
}

/// A sync child shares the parent's cancel token (a linked clone), so
/// cancelling the parent cancels the child -- an ESC on the parent must
/// propagate to a blocking sync child.
#[tokio::test]
async fn test_spawn_sync_links_cancel() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::text("ok"));
    let parent = CancellationToken::new();
    let mut req = req_at_depth(parent_sid, store, provider, 0);
    req.parent_cancel = Some(parent.clone());
    let handle = spawn_child(req).await.expect("spawn");
    parent.cancel();
    assert!(
        handle.cancel.is_cancelled(),
        "sync child's cancel must be linked to the parent's"
    );
}

/// An async child gets a fresh unlinked cancel token, so cancelling the
/// parent does not propagate -- async children run on and are killed
/// explicitly via the runtime's kill path, not by a parent ESC.
#[tokio::test]
async fn test_spawn_async_unlinks_cancel() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::text("ok"));
    let parent = CancellationToken::new();
    let mut req = req_at_depth(parent_sid, store, provider, 0);
    req.run_in_background = true;
    req.parent_cancel = Some(parent.clone());
    let handle = spawn_child(req).await.expect("spawn");
    parent.cancel();
    assert!(
        !handle.cancel.is_cancelled(),
        "async child's cancel must stay independent of the parent's"
    );
}
