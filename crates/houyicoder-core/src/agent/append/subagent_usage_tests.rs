//! Tests for folding a delegated child's usage into the session tally.

use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;
use std::sync::Arc;

use crate::agent::{Runner, ToolRegistry, runner_config::RunnerConfig};

fn runner() -> Arc<Runner> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Arc::new(Runner::new(
        store,
        Arc::new(crate::provider::test_support::FakeProvider::text("done")),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            ..RunnerConfig::default()
        },
    ))
}

fn delegation_result(input: u32, output: u32, cache_read: u32) -> serde_json::Value {
    serde_json::json!({
        "agentId": "child-1",
        "status": "completed",
        "content": "done",
        "usage": {
            "input_tokens": input,
            "output_tokens": output,
            "cache_read_input_tokens": cache_read,
            "cache_write_input_tokens": 0,
            "reasoning_tokens": 0,
        },
    })
}

/// A delegation result carries the child's own usage, which the session tally
/// has to include: otherwise the session understates what it spent.
#[tokio::test]
async fn test_delegated_usage_counted() {
    let runner = runner();
    let session = SessionId::new();
    runner
        .append_tool_result(
            session,
            "c1".into(),
            "agent",
            delegation_result(18_000, 400, 17_000),
            0,
        )
        .await
        .expect("result appended");
    let cumulative = runner.status_snapshot().cumulative_usage;
    assert_eq!(cumulative.input_tokens, 18_000, "child input counts");
    assert_eq!(cumulative.output_tokens, 400);
    assert_eq!(cumulative.cache_read_input_tokens, 17_000);
    assert_eq!(
        cumulative.total_tokens, 18_400,
        "and the total covers both directions"
    );
}

/// The child's input is not this session's context occupancy: it ran in its own
/// window, so folding it into the footprint would report a window the session
/// is not using.
#[tokio::test]
async fn test_delegated_usage_not_occupancy() {
    let runner = runner();
    let session = SessionId::new();
    runner
        .append_tool_result(
            session,
            "c1".into(),
            "agent",
            delegation_result(18_000, 400, 17_000),
            0,
        )
        .await
        .expect("result appended");
    assert_eq!(
        runner.status_snapshot().last_input_tokens,
        0,
        "no parent call ran, so the window footprint is untouched"
    );
}

/// A tool that is not a delegation reports no child usage, whatever its result
/// happens to carry.
#[tokio::test]
async fn test_non_delegation_usage_ignored() {
    let runner = runner();
    let session = SessionId::new();
    runner
        .append_tool_result(
            session,
            "c1".into(),
            "bash",
            delegation_result(18_000, 400, 17_000),
            0,
        )
        .await
        .expect("result appended");
    assert_eq!(
        runner.status_snapshot().cumulative_usage.input_tokens,
        0,
        "only a delegation's usage is the session's cost"
    );
}

/// A delegation result without a usage block leaves the tally alone rather than
/// recording zeroes.
#[tokio::test]
async fn test_delegation_without_usage_ignored() {
    let runner = runner();
    let session = SessionId::new();
    runner
        .append_tool_result(
            session,
            "c1".into(),
            "agent",
            serde_json::json!({"agentId": "child-1", "status": "failed"}),
            0,
        )
        .await
        .expect("result appended");
    assert_eq!(runner.status_snapshot().cumulative_usage.input_tokens, 0);
    // The result itself is still durable: the delegation happened.
    let events = runner.store().trajectory_snapshot(session);
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::ToolResult { .. })),
        "the tool result is recorded either way"
    );
}

/// The folded usage reaches the cumulative tally through the accumulator's own
/// path, so a parent call and a child both land in the same session figure.
#[tokio::test]
async fn test_delegated_adds_to_parent() {
    let runner = runner();
    let session = SessionId::new();
    runner
        .run(session, "hi".into())
        .await
        .expect("run completes");
    let after_parent = runner.status_snapshot().cumulative_usage;
    let parent_input = after_parent.input_tokens;
    runner
        .append_tool_result(
            session,
            "c1".into(),
            "agent",
            delegation_result(5_000, 100, 4_000),
            0,
        )
        .await
        .expect("result appended");
    let combined = runner.status_snapshot().cumulative_usage;
    assert_eq!(
        combined.input_tokens,
        parent_input + 5_000,
        "the child's input adds to the parent's"
    );
}

/// The parser reads only a well-formed usage block: a malformed field counts as
/// zero rather than failing the whole read, and a result with no block reports
/// nothing at all.
#[test]
fn test_usage_block_parsing() {
    let usage = crate::agent::append::subagent_usage::subagent_usage(&serde_json::json!({
        "usage": {
            "input_tokens": 120,
            "output_tokens": 8,
            "cache_read_input_tokens": 100,
            "cache_write_input_tokens": 0,
            "reasoning_tokens": 3,
        }
    }))
    .expect("a well-formed block is read");
    assert_eq!(usage.input_tokens, 120);
    assert_eq!(usage.output_tokens, 8);
    assert_eq!(usage.cache_read_input_tokens, 100);
    assert_eq!(usage.reasoning_tokens, 3);
    assert_eq!(usage.total_tokens, 128, "the total covers both directions");
    assert_eq!(
        usage.non_cached_input_tokens, 20,
        "the cached part is not also counted as fresh input"
    );

    // A block with a missing or wrongly typed field reports zero for it, not a
    // failed read: a partial result is still a result.
    let partial = crate::agent::append::subagent_usage::subagent_usage(&serde_json::json!({
        "usage": {"input_tokens": "not a number", "output_tokens": 5}
    }))
    .expect("a partial block is still read");
    assert_eq!(partial.input_tokens, 0);
    assert_eq!(partial.output_tokens, 5);

    // No block at all means the child reported nothing.
    assert!(
        crate::agent::append::subagent_usage::subagent_usage(&serde_json::json!({"status": "ok"}))
            .is_none()
    );
}
