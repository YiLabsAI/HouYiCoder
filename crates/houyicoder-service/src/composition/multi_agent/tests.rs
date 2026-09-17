use super::*;
use houyicoder_api::tool::Tool;
use houyicoder_async::CancellationToken;
use houyicoder_async::bus::MessageBus;
use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_core::agent::multi_agent::bus_types::{
    BusMessage, ChildStatus, global_completed_topic, permission_request_topic,
    permission_response_topic,
};
use houyicoder_core::agent::multi_agent::registry::BuiltInRegistry;
use houyicoder_core::agent::multi_agent::registry::built_in_all;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_memory::{InMemoryBackend, InMemoryDescriptorStore};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::{LlmEvent, ModelCapabilities, OutputItem};
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use serde_json::Value;
use std::time::Duration;

fn runtime_with_text_child(text: &str) -> (MultiAgentRuntime, Arc<SessionStore>, SessionId) {
    runtime_with(
        Arc::new(FakeProvider::text(text)),
        None,
        ToolRegistry::new(),
    )
}

/// Build a runtime over a chosen provider, bus, and tool set, returning the
/// shared store so a test can read the parent log.
fn runtime_with(
    provider: Arc<dyn ModelProvider>,
    bus: Option<Arc<AgentBus>>,
    tools: ToolRegistry,
) -> (MultiAgentRuntime, Arc<SessionStore>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(built_in_all()));
    let config = RunnerConfig::default();
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store: store.clone(),
        provider,
        tools,
        config,
        worktree_controller: None,
        workspace: Some(std::path::PathBuf::from("/tmp")),
        bus,
        descriptor_store: Some(Arc::new(InMemoryDescriptorStore::new())),
    });
    let parent_sid = SessionId::new();
    (runtime, store, parent_sid)
}

#[tokio::test]
async fn test_foreground_spawn_reaches_terminal() {
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
            .any(|e| matches!(e.event, SessionEvent::SubagentSpawn { .. })),
        "parent log must record the SubagentSpawn boundary",
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "parent log must record the SubagentReturn boundary",
    );
}

/// The child's recorded conversation holds the task and nothing the host
/// injected. Project memory used to be prepended to the child's first user
/// message, so a whole memory file was recorded as if the delegation had
/// written it, and the parent's inline view showed that file above the task
/// -- identically for every child, which read as the view repeating itself.
#[tokio::test]
async fn test_child_task_excludes_memory() {
    let dir = std::env::temp_dir().join(format!("child-task-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("AGENTS.md"), "MEMORYMARKER project rules").expect("write");
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("child answer"));
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(built_in_all()));
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store: store.clone(),
        provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig::default(),
        worktree_controller: None,
        workspace: Some(dir.clone()),
        bus: None,
        descriptor_store: Some(Arc::new(InMemoryDescriptorStore::new())),
    });
    let parent_sid = SessionId::new();
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    // general-purpose is the type that carries project memory (explore and
    // plan omit it), so it is the type that could leak it into the task.
    let args = SpawnArgs::new("general-purpose", "find the auth module", "find auth");
    let outcome = runtime.spawn(&ctx, args).await.expect("spawn");
    let child_sid =
        SessionId::from_display_string(&outcome.child_session_id).expect("child sid parses");
    let text: String = store
        .trajectory_snapshot(child_sid)
        .iter()
        .map(|e| format!("{:?}", e.event))
        .collect();
    assert!(
        text.contains("find the auth module"),
        "the task is recorded: {text}"
    );
    assert!(
        !text.contains("MEMORYMARKER"),
        "project memory must not enter the child's conversation: {text}"
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
        descriptor_store: Some(Arc::new(InMemoryDescriptorStore::new())),
    });
    let parent_sid = SessionId::new();
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "task", "task");
    let outcome = runtime.spawn(&ctx, args).await.expect("spawn");
    assert_eq!(outcome.status.as_deref(), Some("max_turns"));
    assert_eq!(outcome.summary.as_deref(), Some("halfway findings"));
}

#[tokio::test]
async fn test_foreground_spawn_rejects_unknown() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("x");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("no-such-type", "task", "task");
    let err = runtime.spawn(&ctx, args).await.unwrap_err();
    assert!(matches!(err, SpawnFailure::UnknownAgent));
}

#[tokio::test]
async fn test_background_spawn_records_return() {
    let (runtime, store, parent_sid) = runtime_with_text_child("x");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;
    let outcome = runtime
        .spawn(&ctx, args)
        .await
        .expect("background spawn launches, not refused");
    assert!(
        outcome.status.is_none(),
        "background spawn returns no terminal status (it lands later via the bus)"
    );
    assert!(
        !outcome.child_session_id.is_empty(),
        "background spawn returns a child session id"
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
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. }));
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

/// An background spawn of an unknown agent type rejects with UnknownAgent
/// before any detached task starts — the resolve gates both paths.
#[tokio::test]
async fn test_background_spawn_rejects_unknown() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("x");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("nonexistent", "task", "task");
    args.run_in_background = true;
    let err = runtime.spawn(&ctx, args).await.unwrap_err();
    assert!(matches!(err, SpawnFailure::UnknownAgent));
}

/// A foreground spawn announces on the spawned topic so a fleet watcher can
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
        descriptor_store: None,
    });
    let mut rx = bus.subscribe(spawned_topic());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "find auth", "find auth");
    let _outcome = runtime.spawn(&ctx, args).await.expect("spawn");
    match rx.try_recv().expect("spawn announced") {
        BusMessage::Spawned { child } => {
            assert!(!child.agent_id.is_empty());
            assert_eq!(child.agent_type, "explore");
            assert_eq!(child.run_mode, ChildRunMode::Foreground);
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
    use houyicoder_context::SessionEvent;

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
        descriptor_store: Some(Arc::new(InMemoryDescriptorStore::new())),
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
        .find(|e| matches!(e.event, SessionEvent::SubagentSpawn { .. }))
        .expect("first-party spawn writes the boundary");
    let recorded = match &spawn.event {
        SessionEvent::SubagentSpawn { trigger_source, .. } => trigger_source.clone(),
        _ => unreachable!("matched above"),
    };
    assert_eq!(
        recorded, "system:review_gate",
        "first-party spawn stamps the system trigger origin, not a model delegation"
    );
}

/// A first-party background spawn reports its start and records the system
/// trigger when the detached driver runs.
#[tokio::test]
async fn test_background_spawn_records_trigger() {
    use houyicoder_context::SessionEvent;
    use houyicoder_core::agent::multi_agent::spawn::TriggerSource;

    let (runtime, store, parent_sid) = runtime_with_text_child("ok");
    let mut args = SpawnArgs::new("explore", "review the diff", "review the diff");
    args.run_in_background = true;
    let outcome = runtime
        .spawn_system(parent_sid, "review_gate", args)
        .await
        .expect("background spawn");
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
            .any(|e| matches!(e.event, SessionEvent::SubagentSpawn { .. }));
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
        .find(|e| matches!(e.event, SessionEvent::SubagentSpawn { .. }))
        .expect("boundary present");
    let recorded = match &spawn.event {
        SessionEvent::SubagentSpawn { trigger_source, .. } => trigger_source.clone(),
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
        descriptor_store: None,
    });
    runtime
        .send_to_child_inbox("c1", "focus on auth".into())
        .expect("inbox registered");
    match rx.try_recv().expect("steering text delivered") {
        BusMessage::Inbox { text } => assert_eq!(text, "focus on auth"),
        other => panic!("expected Inbox, got {other:?}"),
    }
}

/// A recording HookFire for asserting run_foreground_spawn fires SubagentStart
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

/// A spawn stopped while it waits for a concurrency slot gives the queue
/// slot back. The dispatcher drops the spawn call on a stop, and the gate
/// lives as long as the parent, so a wait that kept its slot would fill the
/// queue a few stops at a time and refuse every later spawn.
#[tokio::test]
async fn test_dropped_spawn_returns_slot() {
    let gate = Arc::new(ConcurrencyGate::new(0, 1));
    let (runtime, _store, parent_sid) = runtime_with_text_child("child answer");
    let runtime = runtime.with_gate(Arc::clone(&gate));
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "task", "task");
    let call = tokio::spawn(async move { runtime.spawn(&ctx, args).await });
    for _ in 0..200 {
        if gate.queued_count() == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(gate.queued_count(), 1, "the spawn waits for a slot");
    call.abort();
    drop(call.await);
    assert_eq!(
        gate.queued_count(),
        0,
        "a stopped spawn leaves no queue slot behind"
    );
}

/// A saturated gate rejects a background spawn without waiting.
#[tokio::test]
async fn test_background_spawn_rejects_saturation() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("x");
    let runtime = runtime.with_gate(std::sync::Arc::new(ConcurrencyGate::new(0, 0)));
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;
    let err = runtime.spawn(&ctx, args).await.unwrap_err();
    assert!(
        matches!(err, SpawnFailure::ConcurrencySaturated),
        "zero-cap gate must reject the background spawn too, got {err:?}"
    );
}

/// run_foreground_spawn fires SubagentStart at the spawn boundary (after
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

/// End-to-end background spawn → detached driver → bus Completed → notification
/// injector → parent queue. The detached child + its notification land
/// independent of the parent's run lifecycle (async cancel unlinked;
/// notification arrives even though the parent is not running a turn).
#[tokio::test]
async fn test_background_child_notifies_parent() {
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
        descriptor_store: None,
    });
    let parent_sid = SessionId::new();
    let mut args = SpawnArgs::new("explore", "review the diff", "review the diff");
    args.run_in_background = true;
    let outcome = runtime
        .spawn_system(parent_sid, "review_gate", args)
        .await
        .expect("background spawn");
    assert!(
        outcome.status.is_none(),
        "background spawn returns no terminal status"
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
        "background child completion reached the parent notification queue"
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

/// A provider that streams a complete text block then errors mid-stream
/// (before any StepFinish). Exercises the partial-output-on-failure branch:
/// the child produced assistant text before the run failed, and the parent
/// should see both the partial work and the failure reason. FailingProvider
/// errors at the first event (no partial), so the branch's partial path
/// (spawn_exec finalize_child line: Some(p) => "...Partial output:\n{p}") was
/// never exercised.
struct PartialThenFailProvider {
    err: ProviderError,
}

impl PartialThenFailProvider {
    fn new(err: ProviderError) -> Self {
        Self { err }
    }
}

impl ModelProvider for PartialThenFailProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> houyicoder_async::PFut<'_, Result<CompletionResponse, ProviderError>> {
        let err = self.err.clone();
        Box::pin(async move { Err(err) })
    }
    fn stream(
        &self,
        _req: CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<houyicoder_protocol::llm::LlmEvent, ProviderError>>
    {
        use houyicoder_protocol::llm::LlmEvent;
        let err = self.err.clone();
        Box::pin(futures::stream::iter(vec![
            Ok(LlmEvent::TextStart { id: "t1".into() }),
            Ok(LlmEvent::TextDelta {
                id: "t1".into(),
                text: "partial findings".into(),
            }),
            Ok(LlmEvent::TextEnd { id: "t1".into() }),
            Err(err),
        ]))
    }
    fn capabilities(&self) -> houyicoder_protocol::llm::ModelCapabilities {
        houyicoder_protocol::llm::ModelCapabilities::default()
    }
}

/// A foreground child failure reaches the parent result and durable return.
#[tokio::test]
async fn test_foreground_failure_reaches_parent() {
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
        descriptor_store: Some(Arc::new(InMemoryDescriptorStore::new())),
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
    let ret_status = events.iter().find_map(|e| match &e.event {
        SessionEvent::SubagentReturn { status, .. } => Some(status.clone()),
        _ => None,
    });
    assert_eq!(
        ret_status.as_deref(),
        Some("failed"),
        "SubagentReturn records the failed status for replay/audit",
    );
}

/// A foreground stream failure excludes text that never reached the durable
/// child log.
#[tokio::test]
async fn test_foreground_failure_loses_partial() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> =
        Arc::new(PartialThenFailProvider::new(ProviderError::Auth));
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
        descriptor_store: Some(Arc::new(InMemoryDescriptorStore::new())),
    });
    let parent_sid = SessionId::new();
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "find the auth module", "find auth");
    let outcome = runtime.spawn(&ctx, args).await.expect("spawn resolves");
    assert_eq!(
        outcome.status.as_deref(),
        Some("failed"),
        "a mid-stream failure surfaces status=failed",
    );
    assert!(
        outcome.summary.as_ref().is_some_and(|s| s.contains("auth")),
        "the summary carries the failure reason: {:?}",
        outcome.summary,
    );
    // GAP: the partial text ("partial findings") the child emitted before the
    // error is NOT in the summary -- the extractor does not flush in-flight
    // deltas on stream-error. When that is fixed, this flips to assert the
    // partial is carried.
    assert!(
        !outcome
            .summary
            .as_ref()
            .is_some_and(|s| s.contains("partial findings")),
        "partial text is currently lost on mid-stream error (extractor flushes on StepFinish only): {:?}",
        outcome.summary,
    );
    let events = store.trajectory_snapshot(parent_sid);
    let ret_status = events.iter().find_map(|e| match &e.event {
        SessionEvent::SubagentReturn { status, .. } => Some(status.clone()),
        _ => None,
    });
    assert_eq!(
        ret_status.as_deref(),
        Some("failed"),
        "SubagentReturn records the failed status",
    );
}

/// A background child failure reaches the parent notification queue.
#[tokio::test]
async fn test_background_failure_notifies_parent() {
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
        descriptor_store: None,
    });
    let parent_sid = SessionId::new();
    let mut args = SpawnArgs::new("explore", "review the diff", "review the diff");
    args.run_in_background = true;
    let outcome = runtime
        .spawn_system(parent_sid, "review_gate", args)
        .await
        .expect("background spawn launches");
    assert!(
        outcome.status.is_none(),
        "background spawn returns no terminal status",
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
        "a failed background child reaches the parent notification queue",
    );
    // The durable return records the background child's terminal status.
    let events = store.trajectory_snapshot(parent_sid);
    let ret_status = events.iter().find_map(|e| match &e.event {
        SessionEvent::SubagentReturn { status, .. } => Some(status.clone()),
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
    let runner = Arc::new(Runner::new(
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

/// kill_child aborts registered children and rejects stale or unknown entries.
#[test]
fn test_kill_child_registry() {
    use houyicoder_core::agent::Runner;
    let (runtime, _store, _parent_sid) = runtime_with_text_child("ok");
    let runner = Arc::new(Runner::new(
        runtime.store.clone(),
        Arc::new(FakeProvider::text("ok")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    ));
    runtime.register_child("c1", &runner);
    assert!(
        runtime.kill_child("c1"),
        "a registered live child upgrades + abort fires → true"
    );
    assert!(
        !runtime.kill_child("unknown"),
        "an unknown child returns false"
    );
    drop(runner);
    assert!(
        !runtime.kill_child("c1"),
        "a dropped child's stale Weak returns false"
    );
}

/// kill_all_children aborts every registered live child + returns the count.
/// Dropped (stale Weak) children are skipped, not counted.
#[test]
fn test_kill_all_children_registry() {
    use houyicoder_core::agent::Runner;
    let (runtime, _store, _parent_sid) = runtime_with_text_child("ok");
    let r1 = Arc::new(Runner::new(
        runtime.store.clone(),
        Arc::new(FakeProvider::text("ok")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    ));
    let r2 = Arc::new(r1.clone());
    runtime.register_child("c1", &r1);
    runtime.register_child("c2", &r2);
    let killed = runtime.kill_all_children();
    assert_eq!(killed, 2, "two live children aborted + counted");
    // A second sweep re-aborts already-cancelled tokens (idempotent) while
    // the Arcs are still held, so the count stays the same.
    let again = runtime.kill_all_children();
    assert_eq!(again, 2, "re-abort is idempotent while the Arcs live");
    drop(r1);
    drop(r2);
    let none = runtime.kill_all_children();
    assert_eq!(none, 0, "dropped children are skipped, not counted");
}

/// A provider that never answers: its stream signals the test on first poll
/// and then waits forever, so a child sits inside its model call until a
/// cancel reaches it, and the test knows exactly where the stop landed.
struct StallForeverProvider {
    entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl StallForeverProvider {
    fn new() -> (Arc<Self>, tokio::sync::oneshot::Receiver<()>) {
        let (entered, rx) = tokio::sync::oneshot::channel();
        (
            Arc::new(Self {
                entered: std::sync::Mutex::new(Some(entered)),
            }),
            rx,
        )
    }
}

impl ModelProvider for StallForeverProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(std::future::pending())
    }

    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let entered = self.entered.lock().expect("entered lock").take();
        Box::pin(futures::stream::once(async move {
            if let Some(entered) = entered {
                let _ = entered.send(());
            }
            std::future::pending::<Result<LlmEvent, ProviderError>>().await
        }))
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// Answers the first model call from a script and stalls on every call after
/// it, signalling when the second one starts: that signal is the boundary
/// between "the child resumed after its approval" and "the resumed run is
/// inside its model call", which is the window a stop has to reach.
struct StallAfterFirstProvider {
    first: FakeProvider,
    calls: std::sync::atomic::AtomicU32,
    resumed: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl StallAfterFirstProvider {
    fn new(outputs: Vec<Vec<OutputItem>>) -> (Arc<Self>, tokio::sync::oneshot::Receiver<()>) {
        let (resumed, rx) = tokio::sync::oneshot::channel();
        (
            Arc::new(Self {
                first: FakeProvider::from_outputs(outputs),
                calls: std::sync::atomic::AtomicU32::new(0),
                resumed: std::sync::Mutex::new(Some(resumed)),
            }),
            rx,
        )
    }
}

impl ModelProvider for StallAfterFirstProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(std::future::pending())
    }

    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return self.first.stream(req);
        }
        let resumed = self.resumed.lock().expect("resumed lock").take();
        Box::pin(futures::stream::once(async move {
            if let Some(resumed) = resumed {
                let _ = resumed.send(());
            }
            std::future::pending::<Result<LlmEvent, ProviderError>>().await
        }))
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A guarded tool these tests never let run: the pause on its ask is the
/// point, and an execution would mean the ask was skipped.
struct GuardedTool;

impl Tool for GuardedTool {
    fn name(&self) -> &str {
        "guarded_write"
    }

    fn description(&self) -> &str {
        "writes notes, gated behind an approval ask"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }

    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async { Ok(serde_json::json!({"written": true})) })
    }

    fn requires_approval(&self) -> bool {
        true
    }
}

/// A stop that lands before the driver has polled the run must still reach
/// the child: the run installs its cancel token at the top of run(), so
/// polling the run before the cancel branch is what makes that token exist
/// when the stop arrives. The child then interrupts instead of running on to
/// a completion nobody asked for.
#[tokio::test]
async fn test_cancel_before_poll_publishes() {
    let bus = Arc::new(AgentBus::new());
    let (provider, _entered) = StallForeverProvider::new();
    let (runtime, store, parent_sid) =
        runtime_with(provider, Some(Arc::clone(&bus)), ToolRegistry::new());
    let token = CancellationToken::new();
    let ctx = ToolCtx::new("c1")
        .with_session(parent_sid)
        .with_cancel(token.clone());
    let mut completed = bus.subscribe(global_completed_topic());
    token.cancel();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        runtime.spawn(&ctx, SpawnArgs::new("explore", "find auth", "find auth")),
    )
    .await
    .expect("a stop before the driver's first poll still ends the child")
    .expect("the spawn reports a terminal outcome");
    assert_eq!(
        outcome.status.as_deref(),
        Some("interrupted"),
        "the stop reached the child instead of letting it run on"
    );
    let message = tokio::time::timeout(Duration::from_secs(5), completed.recv())
        .await
        .expect("a child stopped before its first poll publishes its terminal status")
        .expect("the bus stays open");
    assert!(
        matches!(
            message,
            BusMessage::Completed {
                status: ChildStatus::Killed,
                ..
            }
        ),
        "the stop reaches the fleet as a kill: {message:?}"
    );
    assert!(
        store
            .trajectory_snapshot(parent_sid)
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "the parent log records the return boundary for the stopped child"
    );
}

/// A cancel landing while a foreground child is mid-model-call must not
/// abandon it: the child runs on the parent's cancel token, so it ends
/// interrupted, and the fleet still learns that it ended. The parent's
/// dispatch races the call against the run token and drops the call future,
/// which is what this test does to the spawn future.
#[tokio::test]
async fn test_cancel_mid_call_publishes() {
    let bus = Arc::new(AgentBus::new());
    let (provider, entered) = StallForeverProvider::new();
    let (runtime, store, parent_sid) =
        runtime_with(provider, Some(Arc::clone(&bus)), ToolRegistry::new());
    let token = CancellationToken::new();
    let ctx = ToolCtx::new("c1")
        .with_session(parent_sid)
        .with_cancel(token.clone());
    let mut completed = bus.subscribe(global_completed_topic());
    let mut call = runtime.spawn(&ctx, SpawnArgs::new("explore", "find auth", "find auth"));
    tokio::select! {
        _ = entered => {}
        result = &mut call => panic!("the call returned before the child called the model: {result:?}"),
    }
    token.cancel();
    drop(call);
    let message = tokio::time::timeout(Duration::from_secs(5), completed.recv())
        .await
        .expect("a cancelled child publishes its terminal status")
        .expect("the bus stays open");
    assert!(
        matches!(
            message,
            BusMessage::Completed {
                status: ChildStatus::Killed,
                ..
            }
        ),
        "the cancel reaches the fleet as a kill: {message:?}"
    );
    assert!(
        store
            .trajectory_snapshot(parent_sid)
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "the parent log records the return boundary for the cancelled child"
    );
}

/// A cancel landing while a foreground child waits on an approval ask still
/// ends the child: the ask's route returns nothing on the cancel, and the
/// child resumes only to reach its interrupted terminal. Without that resume
/// the run stays parked on a decision nobody will send, and the fleet row
/// never clears.
#[tokio::test]
async fn test_cancel_mid_ask_publishes() {
    let bus = Arc::new(AgentBus::new());
    let provider = Arc::new(FakeProvider::from_outputs(vec![vec![
        OutputItem::ToolCall {
            id: "tc1".into(),
            name: "guarded_write".into(),
            input: serde_json::json!({"path": "notes.md"}),
        },
    ]]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool));
    let (runtime, store, parent_sid) = runtime_with(provider, Some(Arc::clone(&bus)), tools);
    let token = CancellationToken::new();
    let ctx = ToolCtx::new("c1")
        .with_session(parent_sid)
        .with_cancel(token.clone());
    let mut asked = bus.subscribe(permission_request_topic());
    let mut completed = bus.subscribe(global_completed_topic());
    let mut call = runtime.spawn(
        &ctx,
        SpawnArgs::new("explore", "edit the notes", "edit notes"),
    );
    tokio::select! {
        message = asked.recv() => assert!(
            matches!(message, Ok(BusMessage::PermissionRequest { .. })),
            "the child's ask reaches the parent: {message:?}"
        ),
        result = &mut call => panic!("the call returned before the child asked: {result:?}"),
    }
    token.cancel();
    drop(call);
    let message = tokio::time::timeout(Duration::from_secs(5), completed.recv())
        .await
        .expect("a child cancelled on its ask publishes its terminal status")
        .expect("the bus stays open");
    assert!(
        matches!(
            message,
            BusMessage::Completed {
                status: ChildStatus::Killed,
                ..
            }
        ),
        "the cancel reaches the fleet as a kill: {message:?}"
    );
    assert!(
        store
            .trajectory_snapshot(parent_sid)
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "the parent log records the return boundary for the cancelled child"
    );
}

/// Killing a background child leaves its driver to run the child down to a
/// terminal state, so the fleet gets the kill even though the caller that
/// started the child is long gone. The foreground path takes the same shape:
/// the driver owns the child's bookkeeping and outlives the caller.
/// A cancel landing while a resumed child is inside its model call must end
/// it too: the approval decision put the run back on a fresh cancel token,
/// and the driver's cancel branch has to abort that resumed run, not just the
/// one it started. Without it the child keeps working after the parent
/// stopped, and the fleet row never clears.
#[tokio::test]
async fn test_cancel_mid_resume_publishes() {
    let bus = Arc::new(AgentBus::new());
    let (provider, resumed) = StallAfterFirstProvider::new(vec![vec![OutputItem::ToolCall {
        id: "tc1".into(),
        name: "guarded_write".into(),
        input: serde_json::json!({"path": "notes.md"}),
    }]]);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool));
    let (runtime, store, parent_sid) = runtime_with(provider, Some(Arc::clone(&bus)), tools);
    let token = CancellationToken::new();
    let ctx = ToolCtx::new("c1")
        .with_session(parent_sid)
        .with_cancel(token.clone());
    let mut asked = bus.subscribe(permission_request_topic());
    let mut completed = bus.subscribe(global_completed_topic());
    let mut call = runtime.spawn(
        &ctx,
        SpawnArgs::new("explore", "edit the notes", "edit notes"),
    );
    let request = tokio::select! {
        message = asked.recv() => message.expect("the ask reaches the parent"),
        result = &mut call => panic!("the call returned before the child asked: {result:?}"),
    };
    let BusMessage::PermissionRequest {
        child_id, call_id, ..
    } = request
    else {
        panic!("expected a PermissionRequest, got {request:?}")
    };
    bus.publish(
        &permission_response_topic(&child_id, &call_id),
        BusMessage::PermissionResponse {
            call_id,
            approved: true,
            updated_input: None,
            scope: "once".to_string(),
        },
    );
    tokio::select! {
        _ = resumed => {}
        result = &mut call => panic!("the call returned before the resumed run called the model: {result:?}"),
    }
    token.cancel();
    drop(call);
    let message = tokio::time::timeout(Duration::from_secs(5), completed.recv())
        .await
        .expect("a child cancelled after its resume publishes its terminal status")
        .expect("the bus stays open");
    assert!(
        matches!(
            message,
            BusMessage::Completed {
                status: ChildStatus::Killed,
                ..
            }
        ),
        "the cancel reaches the fleet as a kill: {message:?}"
    );
    assert!(
        store
            .trajectory_snapshot(parent_sid)
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "the parent log records the return boundary for the cancelled child"
    );
}

#[tokio::test]
async fn test_background_kill_publishes() {
    let bus = Arc::new(AgentBus::new());
    let (provider, entered) = StallForeverProvider::new();
    let (runtime, _store, parent_sid) =
        runtime_with(provider, Some(Arc::clone(&bus)), ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "find auth", "find auth");
    args.run_in_background = true;
    let outcome = runtime
        .spawn(&ctx, args)
        .await
        .expect("the background child launches");
    let mut completed = bus.subscribe(global_completed_topic());
    entered.await.expect("the child called the model");
    assert!(
        runtime.kill_child(&outcome.child_session_id),
        "the running child is killed"
    );
    let message = tokio::time::timeout(Duration::from_secs(5), completed.recv())
        .await
        .expect("a killed child publishes its terminal status")
        .expect("the bus stays open");
    assert!(
        matches!(
            message,
            BusMessage::Completed {
                status: ChildStatus::Killed,
                ..
            }
        ),
        "the kill reaches the fleet: {message:?}"
    );
}
