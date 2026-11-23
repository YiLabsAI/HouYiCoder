//! The turn cap a child inherits, and the one it does not.

use super::*;
use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_core::agent::DelegationTool;
use std::sync::atomic::{AtomicU32, Ordering};

struct CountProbeTool(Arc<AtomicU32>);

impl Tool for CountProbeTool {
    fn name(&self) -> &str {
        "count_probe"
    }
    fn description(&self) -> &str {
        "counts executions"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Ok(serde_json::json!({"ok": true})) })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn requires_approval(&self) -> bool {
        false
    }
}

/// A child runs under its own definition's turn cap, not the parent's. A
/// parent sized for a long session must not hand a short read-only child the
/// same budget, or one question can spend the whole session's turns.
#[tokio::test]
async fn test_child_turn_cap_wins() {
    let mut def = built_in_all()
        .into_iter()
        .find(|d| d.subagent_type == "explore")
        .expect("the explore definition is registered");
    def.max_turns = Some(2);
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(vec![def]));
    let count = Arc::new(AtomicU32::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(CountProbeTool(Arc::clone(&count))));
    let call = |n: usize| CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: format!("call_{n}"),
            name: "count_probe".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![
        call(1),
        call(2),
        call(3),
        call(4),
        call(5),
    ]));
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let child_log = store.clone();
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store,
        provider,
        tools,
        config: RunnerConfig {
            max_turns: 5,
            ..RunnerConfig::default()
        },
        worktree_controller: None,
        workspace: Some(PathBuf::from("/tmp")),
        bus: None,
        descriptor_store: Some(Arc::new(InMemoryDescriptorStore::new())),
        skill_registry: None,
        conditional: None,
        hooks: None,
        launcher: Arc::new(StdProcessLauncher::new()),
        mode_gate: None,
    });
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    let args = SpawnArgs::new("explore", "task", "task");

    let outcome = runtime.spawn(&ctx, args).await.expect("spawn");
    let sid = SessionId::from_display_string(&outcome.child_session_id).expect("sid");
    let turns = child_log
        .trajectory_tail(sid)
        .iter()
        .filter(|record| matches!(record.event, SessionEvent::TurnStarted { .. }))
        .count();

    assert_eq!(outcome.status.as_deref(), Some("max_turns"));
    assert_eq!(count.load(Ordering::Relaxed), 2, "both working turns run");
    assert_eq!(
        turns, 3,
        "two working turns plus one wrap-up turn, not five"
    );
}

/// Exhausting all N working turns grants one extra tool-disabled wrap-up turn
/// and preserves max_turns status with a continuation hint on explore.
#[tokio::test]
async fn test_turn_cap_wraps_up() {
    let mut def = built_in_all()
        .into_iter()
        .find(|d| d.subagent_type == "explore")
        .expect("the explore definition is registered");
    def.max_turns = Some(2);
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(vec![def]));
    let count = Arc::new(AtomicU32::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(CountProbeTool(Arc::clone(&count))));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "call_1".into(),
                name: "count_probe".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "call_2".into(),
                name: "count_probe".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "Findings: audited module B".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ]));
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let child_log = store.clone();
    let runtime = Arc::new(MultiAgentRuntime::new(MultiAgentDeps {
        registry: Arc::clone(&registry),
        store,
        provider,
        tools,
        config: RunnerConfig::default(),
        worktree_controller: None,
        workspace: Some(PathBuf::from("/tmp")),
        bus: None,
        descriptor_store: Some(Arc::new(InMemoryDescriptorStore::new())),
        skill_registry: None,
        conditional: None,
        hooks: None,
        launcher: Arc::new(StdProcessLauncher::new()),
        mode_gate: None,
    }));
    let tool = DelegationTool::new(registry);
    let ctx = ToolCtx::new("c1")
        .with_session(SessionId::new())
        .with_spawn_handle(runtime);

    let out = tool
        .execute(
            ctx,
            serde_json::json!({
                "subagent_type": "explore",
                "description": "task",
                "prompt": "task",
            }),
        )
        .await
        .expect("delegation succeeds");
    assert_eq!(count.load(Ordering::Relaxed), 2);
    assert_eq!(out["status"], "max_turns");
    assert_eq!(out["content"], "Findings: audited module B");
    let child_id = out["agentId"].as_str().expect("agentId");
    let continuation = out["continuation"].as_str().expect("continuation hint");
    assert!(continuation.contains(&format!("resume=\"{child_id}\"")));

    let sid = SessionId::from_display_string(child_id).expect("sid");
    let events = child_log.replay(sid).await.expect("replay");
    assert!(events.iter().any(|ev| matches!(
        &ev.event,
        SessionEvent::MetaUser { text } if text.contains("turn budget")
    )));
}

/// A single-turn subagent still runs its working turn before entering the
/// tool-disabled wrap-up turn.
#[tokio::test]
async fn test_single_turn_cap_wraps() {
    let mut def = built_in_all()
        .into_iter()
        .find(|d| d.subagent_type == "explore")
        .expect("the explore definition is registered");
    def.max_turns = Some(1);
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(vec![def]));
    let count = Arc::new(AtomicU32::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(CountProbeTool(Arc::clone(&count))));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "call_1".into(),
                name: "count_probe".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "Findings: single turn".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ]));
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store,
        provider,
        tools,
        config: RunnerConfig::default(),
        worktree_controller: None,
        workspace: Some(PathBuf::from("/tmp")),
        bus: None,
        descriptor_store: Some(Arc::new(InMemoryDescriptorStore::new())),
        skill_registry: None,
        conditional: None,
        hooks: None,
        launcher: Arc::new(StdProcessLauncher::new()),
        mode_gate: None,
    });
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    let outcome = runtime
        .spawn(&ctx, SpawnArgs::new("explore", "task", "task"))
        .await
        .expect("spawn");
    assert_eq!(count.load(Ordering::Relaxed), 1);
    assert_eq!(outcome.status.as_deref(), Some("max_turns"));
    assert_eq!(outcome.summary.as_deref(), Some("Findings: single turn"));
}
