use super::*;
use houyicoder_context::{SessionId, TurnEventKind};
use houyicoder_core::agent::multi_agent::registry::BuiltInRegistry;
use houyicoder_core::agent::multi_agent::registry::built_in_all;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_memory::InMemoryBackend;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;

fn runtime_with_text_child(text: &str) -> (MultiAgentRuntime, Arc<SessionStore>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text(text));
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(built_in_all()));
    let config = RunnerConfig::default();
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store: store.clone(),
        provider,
        tools: ToolRegistry::new(),
        config,
        worktree_controller: None,
        workspace: Some(std::path::PathBuf::from("/tmp")),
        bus: None,
    });
    let parent_sid = SessionId::new();
    (runtime, store, parent_sid)
}

#[tokio::test]
async fn test_sync_spawn_drives_terminal() {
    let (runtime, store, parent_sid) = runtime_with_text_child("child answer");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "find the auth module", "find auth");
    let outcome = runtime.spawn(&ctx, args).await.expect("spawn");
    assert_eq!(outcome.status.as_deref(), Some("completed"));
    assert_eq!(outcome.summary.as_deref(), Some("child answer"));
    // The parent log carries the durable spawn + return boundary pair so
    // replay reconstructs the delegation.
    let events = store.trajectory_snapshot(parent_sid);
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, TurnEventKind::SubagentSpawn { .. })),
        "parent log must record the SubagentSpawn boundary",
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, TurnEventKind::SubagentReturn { .. })),
        "parent log must record the SubagentReturn boundary",
    );
}

#[tokio::test]
async fn test_max_turns_surfaces_partial() {
    // A child that emits text then keeps calling tools past the cap
    // surfaces its last assistant text as the partial result, not an
    // empty summary.
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let resp = houyicoder_protocol::llm::CompletionResponse {
        output: vec![
            houyicoder_protocol::llm::OutputItem::Text {
                text: "halfway findings".into(),
            },
            houyicoder_protocol::llm::OutputItem::ToolCall {
                id: "call_1".into(),
                name: "grep".into(),
                input: serde_json::json!({}),
            },
        ],
        usage: houyicoder_protocol::llm::Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![resp]));
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(built_in_all()));
    let config = RunnerConfig {
        max_turns: 1,
        ..RunnerConfig::default()
    };
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store,
        provider,
        tools: ToolRegistry::new(),
        config,
        worktree_controller: None,
        workspace: Some(std::path::PathBuf::from("/tmp")),
        bus: None,
    });
    let parent_sid = SessionId::new();
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "task", "task");
    let outcome = runtime.spawn(&ctx, args).await.expect("spawn");
    assert_eq!(outcome.status.as_deref(), Some("max_turns"));
    assert_eq!(outcome.summary.as_deref(), Some("halfway findings"));
}

#[tokio::test]
async fn test_sync_spawn_unknown_type() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("x");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("no-such-type", "task", "task");
    let err = runtime.spawn(&ctx, args).await.unwrap_err();
    assert!(matches!(err, SpawnFailure::UnknownAgent));
}

#[tokio::test]
async fn test_async_spawn_launches() {
    let (runtime, store, parent_sid) = runtime_with_text_child("x");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;
    let outcome = runtime
        .spawn(&ctx, args)
        .await
        .expect("async spawn launches, not refused");
    assert!(
        outcome.status.is_none(),
        "async spawn returns no terminal status (it lands later via the bus)"
    );
    assert!(
        !outcome.child_session_id.is_empty(),
        "async spawn returns a child session id"
    );
    // The detached driver runs the child to completion and records the
    // SubagentReturn boundary in the parent log. Yield to let the
    // background task run, then poll until the boundary lands.
    let mut found = false;
    for _ in 0..200 {
        tokio::task::yield_now().await;
        let has_return = store
            .trajectory_snapshot(parent_sid)
            .iter()
            .any(|e| matches!(e.kind, TurnEventKind::SubagentReturn { .. }));
        if has_return {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "detached driver recorded SubagentReturn in the parent log"
    );
}

/// An async spawn of an unknown agent type rejects with UnknownAgent
/// before any detached task starts — the resolve gates both paths.
#[tokio::test]
async fn test_async_spawn_unknown_type() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("x");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("nonexistent", "task", "task");
    args.run_in_background = true;
    let err = runtime.spawn(&ctx, args).await.unwrap_err();
    assert!(matches!(err, SpawnFailure::UnknownAgent));
}

/// A sync spawn announces on the spawned topic so a fleet watcher can
/// subscribe to the child's progress before the first turn lands.
#[tokio::test]
async fn test_spawn_announces_on_bus() {
    use houyicoder_async::bus::MessageBus;
    use houyicoder_core::agent::multi_agent::bus_types::{AgentBus, BusMessage, spawned_topic};

    let bus = Arc::new(AgentBus::new());
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(built_in_all()));
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store: store.clone(),
        provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig::default(),
        worktree_controller: None,
        workspace: Some(std::path::PathBuf::from("/tmp")),
        bus: Some(bus.clone()),
    });
    let mut rx = bus.subscribe(spawned_topic());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "find auth", "find auth");
    let _outcome = runtime.spawn(&ctx, args).await.expect("spawn");
    match rx.try_recv().expect("spawn announced") {
        BusMessage::Spawned {
            agent_id,
            subagent_type,
            run_in_background,
        } => {
            assert!(!agent_id.is_empty());
            assert_eq!(subagent_type, "explore");
            assert!(
                !run_in_background,
                "sync spawn must announce run_in_background=false"
            );
        }
        other => panic!("expected Spawned, got {other:?}"),
    }
}

/// First-party spawn (the service/hook entry) stamps the durable
/// SubagentSpawn boundary with the system trigger origin so a replay
/// distinguishes a flow-driven spawn from a model delegation. The child
/// runs the same narrowed pipeline; the only difference is the trigger.
#[tokio::test]
async fn test_spawn_system_records_trigger() {
    use houyicoder_context::TurnEventKind;

    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(built_in_all()));
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store: store.clone(),
        provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig::default(),
        worktree_controller: None,
        workspace: Some(std::path::PathBuf::from("/tmp")),
        bus: None,
    });
    let args = SpawnArgs::new("explore", "review the diff", "review the diff");
    let outcome = runtime
        .spawn_system(parent_sid, "review_gate", args)
        .await
        .expect("spawn");
    assert_ne!(
        outcome.child_session_id,
        parent_sid.to_string(),
        "child session is distinct from the parent"
    );
    let events = store.trajectory_snapshot(parent_sid);
    let spawn = events
        .iter()
        .find(|e| matches!(e.kind, TurnEventKind::SubagentSpawn { .. }))
        .expect("first-party spawn writes the boundary");
    let recorded = match &spawn.kind {
        TurnEventKind::SubagentSpawn { trigger_source, .. } => trigger_source.clone(),
        _ => unreachable!("matched above"),
    };
    assert_eq!(
        recorded, "system:review_gate",
        "first-party spawn stamps the system trigger origin, not a model delegation"
    );
}

/// A first-party async spawn (run_in_background) returns async_launched and
/// records the system trigger on the durable boundary once the detached driver
/// runs. Pins the async branch of the service/hook entry.
#[tokio::test]
async fn test_spawn_system_async_records() {
    use houyicoder_context::TurnEventKind;
    use houyicoder_core::agent::multi_agent::spawn::TriggerSource;

    let (runtime, store, parent_sid) = runtime_with_text_child("ok");
    let mut args = SpawnArgs::new("explore", "review the diff", "review the diff");
    args.run_in_background = true;
    let outcome = runtime
        .spawn_system(parent_sid, "review_gate", args)
        .await
        .expect("async spawn");
    assert!(
        outcome.status.is_none(),
        "async first-party spawn returns no terminal status"
    );
    let mut found = false;
    for _ in 0..200 {
        tokio::task::yield_now().await;
        let has = store
            .trajectory_snapshot(parent_sid)
            .iter()
            .any(|e| matches!(e.kind, TurnEventKind::SubagentSpawn { .. }));
        if has {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "detached driver recorded the first-party spawn boundary"
    );
    let snap = store.trajectory_snapshot(parent_sid);
    let spawn = snap
        .iter()
        .find(|e| matches!(e.kind, TurnEventKind::SubagentSpawn { .. }))
        .expect("boundary present");
    let recorded = match &spawn.kind {
        TurnEventKind::SubagentSpawn { trigger_source, .. } => trigger_source.clone(),
        _ => unreachable!("matched above"),
    };
    assert_eq!(
        recorded,
        TriggerSource::System {
            hook: "review_gate".into()
        }
        .as_durable(),
        "async first-party spawn stamps the system trigger origin"
    );
}

/// send_to_child_inbox routes a steering text into a child's registered
/// inbox on the bus; the child's drive loop drains it at its next turn.
#[tokio::test]
async fn test_send_to_child_inbox() {
    use houyicoder_api::spawn::SpawnHandle;
    use houyicoder_async::bus::MessageBus;
    use houyicoder_core::agent::multi_agent::bus_types::{AgentBus, BusMessage};

    let bus = Arc::new(AgentBus::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BusMessage>();
    bus.register_inbox("c1", tx);
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(built_in_all()));
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store: Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        provider: Arc::new(FakeProvider::text("x")),
        tools: ToolRegistry::new(),
        config: RunnerConfig::default(),
        worktree_controller: None,
        workspace: Some(std::path::PathBuf::from("/tmp")),
        bus: Some(bus),
    });
    runtime
        .send_to_child_inbox("c1", "focus on auth".into())
        .expect("inbox registered");
    match rx.try_recv().expect("steering text delivered") {
        BusMessage::Inbox { text } => assert_eq!(text, "focus on auth"),
        other => panic!("expected Inbox, got {other:?}"),
    }
}

/// A recording HookFire for asserting run_sync_spawn fires SubagentStart
/// and SubagentStop at the durable spawn and return boundaries.
struct RecordingHookFire {
    events: Arc<std::sync::Mutex<Vec<HookEventKind>>>,
}
impl HookFire for RecordingHookFire {
    fn fire(&self, event: HookEventKind, _payload: HookFirePayload) -> PFut<'_, ()> {
        self.events.lock().expect("recorder lock").push(event);
        Box::pin(async {})
    }
}

/// A zero-cap, zero-queue gate proves the gate sits on the spawn path:
/// every spawn rejects with ConcurrencySaturated, not BudgetExceeded and
/// not a successful spawn. If the gate were unwired, the spawn would
/// succeed like the drives-terminal test.
#[tokio::test]
async fn test_spawn_rejected_when_saturated() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("x");
    let runtime = runtime.with_gate(std::sync::Arc::new(ConcurrencyGate::new(0, 0)));
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "task", "task");
    let err = runtime.spawn(&ctx, args).await.unwrap_err();
    assert!(
        matches!(err, SpawnFailure::ConcurrencySaturated),
        "zero-cap gate must reject via the concurrency path, got {err:?}"
    );
}

/// The concurrency cap applies to the async path too: a zero-cap gate rejects
/// a background spawn with ConcurrencySaturated, not a successful async launch.
/// Pins DEFECT-2 — before the fix, the async path bypassed the gate and the
/// spawn succeeded like the launches test.
#[tokio::test]
async fn test_async_spawn_saturated_rejects() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("x");
    let runtime = runtime.with_gate(std::sync::Arc::new(ConcurrencyGate::new(0, 0)));
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;
    let err = runtime.spawn(&ctx, args).await.unwrap_err();
    assert!(
        matches!(err, SpawnFailure::ConcurrencySaturated),
        "zero-cap gate must reject the async spawn too, got {err:?}"
    );
}

/// run_sync_spawn fires SubagentStart at the spawn boundary (after
/// spawn_child, before the run) and SubagentStop at the return boundary
/// (before record_subagent_return), threaded through ToolCtx.hook_fire.
#[tokio::test]
async fn test_spawn_fires_start_stop() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("child answer");
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let ctx = ToolCtx::new("c1")
        .with_session(parent_sid)
        .with_hook_fire(Arc::new(RecordingHookFire {
            events: events.clone(),
        }) as Arc<dyn HookFire>);
    let args = SpawnArgs::new("explore", "find auth", "find auth");
    let outcome = runtime.spawn(&ctx, args).await.expect("spawn");
    assert_eq!(outcome.status.as_deref(), Some("completed"));
    let fired = events.lock().expect("events lock").clone();
    assert!(
        fired.contains(&HookEventKind::SubagentStart),
        "spawn fires SubagentStart: {fired:?}"
    );
    assert!(
        fired.contains(&HookEventKind::SubagentStop),
        "return fires SubagentStop: {fired:?}"
    );
    let start_idx = fired
        .iter()
        .position(|e| *e == HookEventKind::SubagentStart)
        .expect("start fired");
    let stop_idx = fired
        .iter()
        .position(|e| *e == HookEventKind::SubagentStop)
        .expect("stop fired");
    assert!(
        start_idx < stop_idx,
        "SubagentStart fires before SubagentStop: {fired:?}"
    );
}

/// End-to-end async spawn → detached driver → bus Completed → notification
/// injector → parent queue. The detached child + its notification land
/// independent of the parent's run lifecycle (async cancel unlinked;
/// notification arrives even though the parent is not running a turn).
#[tokio::test]
async fn test_async_spawn_notifies_parent() {
    use houyicoder_core::agent::multi_agent::bus_types::AgentBus;
    use houyicoder_core::agent::{Runner, ToolRegistry};

    let bus = Arc::new(AgentBus::new());
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("done"));
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(built_in_all()));
    let parent_runner = Arc::new(Runner::new(
        store.clone(),
        Arc::clone(&provider),
        ToolRegistry::new(),
        RunnerConfig::default(),
    ));
    super::super::notification_drain::spawn(
        Some(bus.clone()),
        Arc::clone(&parent_runner),
        tokio::runtime::Handle::current(),
    );
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store: store.clone(),
        provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig::default(),
        worktree_controller: None,
        workspace: Some(std::path::PathBuf::from("/tmp")),
        bus: Some(bus.clone()),
    });
    let parent_sid = SessionId::new();
    let mut args = SpawnArgs::new("explore", "review the diff", "review the diff");
    args.run_in_background = true;
    let outcome = runtime
        .spawn_system(parent_sid, "review_gate", args)
        .await
        .expect("async spawn");
    assert!(
        outcome.status.is_none(),
        "async spawn returns no terminal status"
    );
    let mut found = false;
    for _ in 0..200 {
        tokio::task::yield_now().await;
        if !parent_runner.queued_notifications_snapshot().is_empty() {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "async child completion reached the parent notification queue"
    );
    let notif = &parent_runner.queued_notifications_snapshot()[0];
    assert!(notif.contains("explore"), "carries the subagent type");
    assert!(notif.contains("done"), "carries the child summary");
}

/// A provider whose stream yields one non-retryable error then ends, so a
/// child run fails fast through the real stream path: Auth is not retryable,
/// so the drive loop maps it to ProviderFatal and terminates without retry or
/// backoff. The canned fake only errors the complete path, not the stream
/// path the runner takes, so this struct owns the stream error.
use houyicoder_async::PStream;
use houyicoder_protocol::llm::{CompletionRequest, CompletionResponse, ProviderError};

struct FailingProvider {
    err: ProviderError,
}

impl FailingProvider {
    fn new(err: ProviderError) -> Self {
        Self { err }
    }
}

impl ModelProvider for FailingProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let err = self.err.clone();
        Box::pin(async move { Err(err) })
    }
    fn stream(
        &self,
        _req: CompletionRequest,
    ) -> PStream<'_, Result<houyicoder_protocol::llm::LlmEvent, ProviderError>> {
        let err = self.err.clone();
        Box::pin(futures::stream::once(async move { Err(err) }))
    }
    fn capabilities(&self) -> houyicoder_protocol::llm::ModelCapabilities {
        houyicoder_protocol::llm::ModelCapabilities::default()
    }
}

/// A sync child whose run fails (non-retryable provider error) surfaces the
/// failure to the parent as the spawn tool result: status=failed + a
/// non-empty summary carrying the error. The durable SubagentReturn boundary
/// lands in the parent log with the failed status so replay is honest about
/// the outcome. Pins the sync propagation path: the parent learns of a child
/// failure through the tool result, not a silent hang or an empty answer.
#[tokio::test]
async fn test_sync_failed_child_propagates() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FailingProvider::new(ProviderError::Auth));
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(built_in_all()));
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store: store.clone(),
        provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig::default(),
        worktree_controller: None,
        workspace: Some(std::path::PathBuf::from("/tmp")),
        bus: None,
    });
    let parent_sid = SessionId::new();
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "find the auth module", "find auth");
    let outcome = runtime.spawn(&ctx, args).await.expect("spawn resolves");
    assert_eq!(
        outcome.status.as_deref(),
        Some("failed"),
        "a failed child surfaces status=failed to the parent tool result",
    );
    assert!(
        outcome.summary.as_ref().is_some_and(|s| s.contains("auth")),
        "the failure summary carries the error reason so the model can act on it",
    );
    // The durable SubagentReturn boundary records the failure so replay
    // reconstructs the delegation honestly (not a silent drop).
    let events = store.trajectory_snapshot(parent_sid);
    let ret_status = events.iter().find_map(|e| match &e.kind {
        TurnEventKind::SubagentReturn { status, .. } => Some(status.clone()),
        _ => None,
    });
    assert_eq!(
        ret_status.as_deref(),
        Some("failed"),
        "SubagentReturn records the failed status for replay/audit",
    );
}

/// An async (detached) child whose run fails notifies the parent through the
/// bus completion path: the run emits RunCompleted with a failed status, the
/// bus bridge maps it to ChildStatus::Failed, and the notification injector
/// enqueues a lower-priority message the parent reads mid-turn. Pins the
/// async propagation path: a background child failure reaches the parent even
/// though the parent is not blocked on the spawn — the parent learns of the
/// failure, not a perpetual running pill.
#[tokio::test]
async fn test_async_failed_child_notifies() {
    use houyicoder_core::agent::multi_agent::bus_types::AgentBus;
    use houyicoder_core::agent::{Runner, ToolRegistry};

    let bus = Arc::new(AgentBus::new());
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let child_provider: Arc<dyn ModelProvider> =
        Arc::new(FailingProvider::new(ProviderError::Auth));
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(built_in_all()));
    // The parent runner is the notification sink; it never runs a turn here,
    // so its provider is a placeholder.
    let parent_runner = Arc::new(Runner::new(
        store.clone(),
        Arc::new(FakeProvider::text("ok")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    ));
    super::super::notification_drain::spawn(
        Some(bus.clone()),
        Arc::clone(&parent_runner),
        tokio::runtime::Handle::current(),
    );
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store: store.clone(),
        provider: child_provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig::default(),
        worktree_controller: None,
        workspace: Some(std::path::PathBuf::from("/tmp")),
        bus: Some(bus.clone()),
    });
    let parent_sid = SessionId::new();
    let mut args = SpawnArgs::new("explore", "review the diff", "review the diff");
    args.run_in_background = true;
    let outcome = runtime
        .spawn_system(parent_sid, "review_gate", args)
        .await
        .expect("async spawn launches");
    assert!(
        outcome.status.is_none(),
        "async spawn returns no terminal status",
    );
    let mut found = false;
    for _ in 0..200 {
        tokio::task::yield_now().await;
        let snap = parent_runner.queued_notifications_snapshot();
        if !snap.is_empty() {
            assert!(
                snap[0].contains("explore"),
                "the failure notification carries the subagent type",
            );
            assert!(
                snap[0].contains("failed"),
                "the failure notification carries the failed status",
            );
            assert!(
                snap[0].contains("auth"),
                "the failure notification carries the error reason, not just the status label",
            );
            found = true;
            break;
        }
    }
    assert!(
        found,
        "a failed async child reaches the parent notification queue",
    );
    // The durable SubagentReturn boundary lands even on the async path — the
    // detached driver runs finalize_child, which records the return with the
    // failed status. The status is pinned, not just the boundary's existence,
    // so a regression that records the wrong terminal on the async path goes
    // red rather than staying green on a status=completed mislabel.
    let events = store.trajectory_snapshot(parent_sid);
    let ret_status = events.iter().find_map(|e| match &e.kind {
        TurnEventKind::SubagentReturn { status, .. } => Some(status.clone()),
        _ => None,
    });
    assert_eq!(
        ret_status.as_deref(),
        Some("failed"),
        "the async failed child records SubagentReturn with status=failed",
    );
}

/// cancel_child_turn upgrades a registered child's Weak to reach the runner
/// (returns true); a dropped child's stale Weak is pruned (returns false);
/// an unknown child is a no-op (returns false). The registry does not leak
/// across a long-lived parent because the stale entry is removed on the
/// failed upgrade.
#[test]
fn test_cancel_child_turn_registry() {
    use houyicoder_core::agent::Runner;
    let (runtime, _store, _parent_sid) = runtime_with_text_child("ok");
    let runner = Arc::new(Runner::with_shared_store(
        runtime.store.clone(),
        Arc::new(FakeProvider::text("ok")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    ));
    runtime.register_child("c1", &runner);
    assert!(
        runtime.cancel_child_turn("c1"),
        "a registered live child upgrades and returns true"
    );
    assert!(
        !runtime.cancel_child_turn("unknown"),
        "an unknown child returns false"
    );
    drop(runner);
    assert!(
        !runtime.cancel_child_turn("c1"),
        "a dropped child's stale Weak returns false"
    );
}
