//! TDD anchor for spawn_child: the minimal contract before implementation.

use super::{SpawnError, SpawnRequest, TriggerSource, spawn_child};
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
        trigger: TriggerSource::ModelTool {
            tool_call_id: "test-call".to_string(),
        },
        depth,
        isolation: IsolationMode::None,
        worktree_controller: None,
        run_in_background: false,
        parent_cancel: None,
        bus: None,
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

    // The child runner carries depth + 1 and the resolved type, so a
    // nested agent call reports its level to the recursion guard.
    let identity = handle.runner.agent_identity();
    assert_eq!(identity.depth, 1, "child depth must be parent + 1");
    assert_eq!(
        identity.subagent_type.as_deref(),
        Some("explore"),
        "child identity must carry the resolved type",
    );
    assert_eq!(
        identity.parent_session_id.as_deref(),
        Some(parent_sid.to_string().as_str()),
    );

    // The parent log must carry a SubagentSpawn boundary whose
    // child_session_id matches the handle's session id. Resume and orphan
    // reconciliation pair spawn with return on this id; a mismatched or
    // missing id breaks the durable chain.
    let parent_events = store.trajectory_snapshot(parent_sid);
    let spawn = parent_events
        .iter()
        .find(|e| matches!(e.kind, TurnEventKind::SubagentSpawn { .. }))
        .expect("parent log must carry the spawn boundary");
    let (recorded_child, recorded_trigger) = match &spawn.kind {
        TurnEventKind::SubagentSpawn {
            child_session_id,
            trigger_source,
            ..
        } => (child_session_id.clone(), trigger_source.clone()),
        _ => unreachable!("matched above"),
    };
    assert_eq!(
        recorded_child,
        handle.session.to_string(),
        "spawn boundary must record the child session id"
    );
    assert_eq!(
        recorded_trigger, "model:test-call",
        "spawn boundary must record the trigger source (a model delegation)"
    );
}

/// A system-triggered spawn (a hook/gate, not the model) records its origin
/// on the SubagentSpawn boundary so a replay distinguishes a gate-driven
/// spawn from a model delegation. Pins the trigger_source durable trail for
/// the first-party spawn entry.
#[tokio::test]
async fn test_spawn_records_system_trigger() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::text("ok"));
    let mut req = req_at_depth(parent_sid, store.clone(), provider, 0);
    req.trigger = TriggerSource::System {
        hook: "review_gate".into(),
    };
    let handle = spawn_child(req).await.expect("spawn should succeed");
    let events = store.trajectory_snapshot(parent_sid);
    let spawn = events
        .iter()
        .find(|e| matches!(e.kind, TurnEventKind::SubagentSpawn { .. }))
        .expect("parent log must carry the spawn boundary");
    let recorded_trigger = match &spawn.kind {
        TurnEventKind::SubagentSpawn { trigger_source, .. } => trigger_source.clone(),
        _ => unreachable!("matched above"),
    };
    assert_eq!(
        recorded_trigger, "system:review_gate",
        "system-triggered spawn records its hook origin, not a model delegation"
    );
    // The child is spawned regardless of the trigger origin; the capability
    // baseline is the parent's, so a system trigger does not bypass the
    // capability intersection rule. The child identity still carries depth+1.
    assert_eq!(
        handle.runner.agent_identity().depth,
        1,
        "system spawn still produces a depth-tracked child"
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

/// Boundary opposite of the depth cap: a spawn at depth MAX-1 (3) is the
/// last allowed level — the child carries depth 4 (MAX), one short of the
/// reject. Pins both sides of the boundary so an off-by-one in the guard
/// flips this red before the reject test does.
#[tokio::test]
async fn test_spawn_depth_under_cap() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::text("ok"));
    let req = req_at_depth(parent_sid, store.clone(), provider, 3);
    let handle = spawn_child(req)
        .await
        .expect("depth MAX-1 must allow spawn");
    assert_eq!(
        handle.runner.agent_identity().depth,
        4,
        "child of a depth-3 parent is depth 4 (MAX) — the last allowed level"
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

/// A spawned child's requests carry the lowest effort tier, so a fan-out of
/// sub-agents does not multiply reasoning-token spend.
#[tokio::test]
async fn test_spawn_child_effort_gate() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::text("ok"));
    let req = req_at_depth(parent_sid, store, provider, 0);
    let handle = spawn_child(req).await.expect("spawn");
    assert_eq!(
        handle.runner.active_effort(),
        Some(houyicoder_protocol::llm::EffortLevel::Low),
        "child must pin the lowest effort tier at spawn"
    );
}

/// A child armed with a bus publishes one Progress snapshot at each
/// turn boundary; a parent subscribed to the child's progress topic receives
/// it. The causal path the acceptance pins: publish → receive (not a timer).
/// The child runs two turns — a tool call (RunAgain) then final text — so
/// the turn-1 boundary fires exactly one Progress before the terminal turn.
#[tokio::test]
async fn test_spawn_publishes_progress() {
    use crate::agent::multi_agent::bus_types::{AgentBus, BusMessage, progress_topic};
    use houyicoder_async::bus::MessageBus;
    use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};

    let bus = Arc::new(AgentBus::new());
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    // Turn 1: a tool call → resolve_turn returns RunAgain → progress emitted.
    // Turn 2: final text → FinalOutput → terminal, no progress.
    let resp1 = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "grep".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage {
            input_tokens: 100,
            output_tokens: 50,
            ..Usage::default()
        },
        model: "test".into(),
    };
    let resp2 = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage {
            input_tokens: 100,
            output_tokens: 50,
            ..Usage::default()
        },
        model: "test".into(),
    };
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::new(vec![resp1, resp2]));
    let mut req = req_at_depth(parent_sid, store, provider, 0);
    req.bus = Some(bus.clone());
    let handle = spawn_child(req).await.expect("spawn");

    // Subscribe before the run: broadcast drops messages a late subscriber
    // never saw. The parent subscribes at spawn time (before the run starts).
    let child_id = handle.session.to_string();
    let mut rx = bus.subscribe(&progress_topic(&child_id));

    // Drive the child to terminal. The publish fires mid-run, but the
    // broadcast buffer holds it for the receiver to drain after.
    let _result = handle
        .runner
        .run(handle.session, "do the task".to_string())
        .await;

    // The turn-1 boundary published one Progress the parent received.
    match rx.try_recv().expect("parent received progress") {
        BusMessage::Progress {
            agent_id,
            turn,
            tokens,
            tool_uses,
            last_activity,
        } => {
            assert_eq!(agent_id, child_id);
            assert_eq!(turn, 1);
            assert_eq!(tokens, 150); // 100 input + 50 output, cumulative after turn 1
            assert_eq!(tool_uses, 1);
            assert_eq!(last_activity.as_deref(), Some("grep"));
        }
        other => panic!("expected Progress, got {other:?}"),
    }
    // Only one turn boundary was crossed; no second Progress.
    assert!(
        rx.try_recv().is_err(),
        "no second progress for a 2-turn run"
    );
}

/// Adversarial: a child that completes in a single turn (text only, no
/// tool calls) crosses no turn boundary, so it publishes no Progress — a
/// parent that saw a message here would mean the guard fires on terminal turns
/// too, flooding the bus with useless completion-coupled progress.
#[tokio::test]
async fn test_spawn_terminal_skips_progress() {
    use crate::agent::multi_agent::bus_types::{AgentBus, progress_topic};
    use houyicoder_async::bus::MessageBus;

    let bus = Arc::new(AgentBus::new());
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::text("immediate answer"));
    let mut req = req_at_depth(parent_sid, store, provider, 0);
    req.bus = Some(bus.clone());
    let handle = spawn_child(req).await.expect("spawn");
    let child_id = handle.session.to_string();
    let mut rx = bus.subscribe(&progress_topic(&child_id));
    let _result = handle
        .runner
        .run(handle.session, "do the task".to_string())
        .await;
    // No turn boundary crossed (single turn → FinalOutput), so no Progress.
    assert!(
        rx.try_recv().is_err(),
        "single-turn child must not publish progress"
    );
}

/// A child that completes publishes a Completed message on its completed
/// topic, so a parent subscribed before the run learns the child is done.
#[tokio::test]
async fn test_spawn_publishes_completion() {
    use crate::agent::multi_agent::bus_types::{
        AgentBus, BusMessage, ChildStatus, completed_topic,
    };
    use houyicoder_async::bus::MessageBus;

    let bus = Arc::new(AgentBus::new());
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::text("found auth module"));
    let mut req = req_at_depth(parent_sid, store, provider, 0);
    req.bus = Some(bus.clone());
    let handle = spawn_child(req).await.expect("spawn");
    let child_id = handle.session.to_string();
    let mut rx = bus.subscribe(&completed_topic(&child_id));
    let _result = handle
        .runner
        .run(handle.session, "do the task".to_string())
        .await;
    match rx.try_recv().expect("parent received completion") {
        BusMessage::Completed {
            agent_id,
            status,
            summary,
        } => {
            assert_eq!(agent_id, child_id);
            assert_eq!(status, ChildStatus::Completed);
            assert_eq!(summary, "found auth module");
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

/// A child drains its bus inbox at each turn boundary: a text the parent
/// sent to the child's inbox lands as a MidTurnInput event before the next
/// model call. The mpsc inbox is point-to-point, so the message queued
/// after spawn + before run is drained at turn 2 — no race.
#[tokio::test]
async fn test_inbox_drained_at_boundary() {
    use crate::agent::multi_agent::bus_types::{AgentBus, BusMessage};
    use houyicoder_async::bus::MessageBus;
    use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};

    let bus = Arc::new(AgentBus::new());
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let turn1 = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "grep".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let turn2 = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::new(vec![turn1, turn2]));
    let mut req = req_at_depth(parent_sid, store.clone(), provider, 0);
    req.bus = Some(bus.clone());
    let handle = spawn_child(req).await.expect("spawn");
    let child_id = handle.session.to_string();
    // Parent steers the child before its second turn; the inbox is
    // registered at spawn so this succeeds, and the drive loop drains it
    // at the turn-2 boundary.
    bus.send_inbox(
        &child_id,
        BusMessage::Inbox {
            text: "steer here".into(),
        },
    )
    .expect("inbox registered at spawn");
    let _result = handle
        .runner
        .run(handle.session, "do the task".to_string())
        .await;
    let events = store.trajectory_snapshot(handle.session);
    assert!(
        events.iter().any(|e| matches!(&e.kind,
                TurnEventKind::MidTurnInput { text } if text == "steer here")),
        "inbox text drained at the turn boundary as a MidTurnInput event"
    );
}
