//! Subtree usage aggregation tests for multi-agent runs.

use super::*;

/// A child that delegated to a grandchild reports the whole subtree, not just
/// its own provider calls: the return the parent records carries the child's
/// own usage plus every return in the child's log.
#[tokio::test]
async fn test_return_usage_covers_subtree() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("x"));
    let runner = Arc::new(Runner::new(
        store.clone(),
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    ));
    let child_sid = SessionId::new();
    // The grandchild's return, as the child's own finalize would have written
    // it into the child's log.
    store
        .append(houyicoder_context::SessionLogEntry {
            id: EventId::new(),
            session: child_sid,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::SubagentReturn {
                child_session_id: "grandchild".into(),
                status: "completed".into(),
                summary: String::new(),
                result_ref: "grandchild".into(),
                input_tokens: 30,
                output_tokens: 4,
                cache_read_input_tokens: 10,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                input_reported: true,
                cache_read_reported: true,
                cache_write_reported: true,
                tool_calls: 0,
                tool_failures: 0,
                duration_ms: 0,
                ledger_measured: true,
                orphaned: false,
            },
        })
        .await
        .unwrap();
    let usage = child_subtree_usage(&runner, child_sid);
    assert_eq!(
        usage.input_tokens, 30,
        "the grandchild's input reaches the parent"
    );
    assert_eq!(usage.output_tokens, 4);
    assert_eq!(usage.cache_read_input_tokens, 10);
}
