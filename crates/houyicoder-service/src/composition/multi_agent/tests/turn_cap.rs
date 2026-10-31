//! The turn cap a child inherits, and the one it does not.

use super::*;

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
    // Every response carries a tool call, so the loop can only stop by
    // reaching the cap. A response with text alone would end on its own and
    // the test would pass without the cap doing anything.
    let call = |n: usize| CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: format!("call_{n}"),
            name: "grep".into(),
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
        tools: ToolRegistry::new(),
        // Deliberately different from the child's cap, so the two answers are
        // distinguishable: a child that stopped at 3 read this number, and one
        // that stopped at 2 read its own definition.
        config: RunnerConfig {
            max_turns: 3,
            ..RunnerConfig::default()
        },
        worktree_controller: None,
        workspace: Some(PathBuf::from("/tmp")),
        bus: None,
        descriptor_store: Some(Arc::new(InMemoryDescriptorStore::new())),
        skill_registry: None,
        conditional: None,
        hooks: None,
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

    assert_eq!(
        outcome.status.as_deref(),
        Some("max_turns"),
        "the cap stops the child"
    );
    // The status alone cannot tell the two caps apart: both end as max_turns.
    // The count of turns started can, and unlike the size of the log it does
    // not move when a later change adds or removes an unrelated record.
    assert_eq!(turns, 2, "two turns of work, not the parent's three");
}
