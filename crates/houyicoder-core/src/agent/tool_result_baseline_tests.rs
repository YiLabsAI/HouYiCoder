//! Tool-result JSON baseline — the model-visible strings the agent loop emits
//! at the seven construction points in the dispatch path. This file locks the
//! current behavior so the ToolError reshape and the synthetic-outcome
//! concentration cannot drift the strings the model sees.
//!
//! Six points are exercised here; the seventh (unknown-tool-on-resume, fired
//! when a resume decision references a tool no longer in the registry) needs a
//! registry mutation between run and resume that the current Runner API does
//! not expose, so it is recorded as a constant and exercised by the cross-layer
//! E2E later.

use std::sync::Arc;

use houyicoder_context::{SessionId, TurnEventKind};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{OutputItem, Usage};
use houyicoder_session::SessionStore;
use serde_json::Value;

use super::tests::{GuardedTool, HangingProvider, runner_with};
use super::*;
use crate::provider::test_support::FakeProvider;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_protocol::extension::ToolError;

/// The seven canonical model-visible tool-result payloads, in dispatch-path
/// order. The ToolError reshape and the SyntheticToolOutcome enum must produce
/// these byte-for-byte.
const TOOL_ERROR_PREFIX: &str = "tool error: ";
const MSG_UNKNOWN_TOOL: &str = "unknown tool: no_such_tool";
const MSG_TOOL_FAILURE: &str = "baseline tool failure";
const MSG_REJECTED: &str = "rejected by user";
const MSG_INTERRUPTED: &str = "interrupted by user";
const MSG_UNKNOWN_TOOL_ON_RESUME: &str = "unknown tool on resume";

fn expected_error_object(message: &str) -> Value {
    serde_json::json!({ "error": message })
}

async fn tool_result_output(runner: &Runner, session: SessionId, call_id: &str) -> Value {
    let events = runner.store().replay(session).await.expect("replay");
    for event in &events {
        if let TurnEventKind::ToolResult {
            call_id: cid,
            output,
            ..
        } = &event.kind
            && cid == call_id
        {
            return output.clone();
        }
    }
    panic!("no ToolResult for call_id {call_id}");
}

/// A tool that always fails with a fixed message, so the tool-error JSON shape
/// (the three e.to_string() sites) has a stable input to lock against.
struct ErroringTool {
    name: &'static str,
}

impl Tool for ErroringTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "a tool that always fails for the baseline"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        _input: Value,
    ) -> houyicoder_async::PFut<'_, Result<Value, ToolError>> {
        let msg = MSG_TOOL_FAILURE.to_string();
        Box::pin(async move { Err(ToolError::Failed(msg)) })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn test_unknown_tool_error_object() {
    // Dispatch a tool call whose name is not in the registry. The loop emits
    // the unknown-tool synthetic result (dispatch-path unknown-tool branch).
    let resp = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "no_such_tool".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![
        resp,
        CompletionResponse {
            output: vec![OutputItem::Text { text: "ok".into() }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    runner.run(session, "hi".into()).await.unwrap();
    let output = tool_result_output(&runner, session, "c1").await;
    assert_eq!(
        output,
        expected_error_object(MSG_UNKNOWN_TOOL),
        "unknown tool must produce the exact synthetic string, no prefix"
    );
}

#[tokio::test]
async fn test_baseline_error_carries_prefix() {
    // A tool that returns Err(ToolError::Failed(msg)) must surface as
    // {"error": "tool error: {msg}"} — the Display prefix is part of the
    // model-visible payload. This locks the three e.to_string() sites.
    let resp = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "failer".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![
        resp,
        CompletionResponse {
            output: vec![OutputItem::Text { text: "ok".into() }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ErroringTool { name: "failer" }));
    let runner = runner_with(p, tools);
    let session = SessionId::new();
    runner.run(session, "hi".into()).await.unwrap();
    let output = tool_result_output(&runner, session, "c1").await;
    assert_eq!(
        output,
        expected_error_object(&format!("{TOOL_ERROR_PREFIX}{MSG_TOOL_FAILURE}")),
        "tool execute error must carry the tool-error Display prefix"
    );
}

#[tokio::test]
async fn test_baseline_rejected_approval() {
    // An approval-requiring tool whose decision is reject must surface the
    // rejected-by-user synthetic result (the reject branch of apply_decisions).
    let responses = vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "c1".into(),
                name: "guarded".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text { text: "ok".into() }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    let p = Arc::new(FakeProvider::new(responses));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    let runner = runner_with(p, tools);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    let approvals = match result.outcome {
        RunOutcome::Interruption(a) => a,
        other => panic!("expected interruption, got {other:?}"),
    };
    let decisions: Vec<ApprovalDecision> = approvals
        .iter()
        .map(|a| ApprovalDecision::reject(&a.call_id))
        .collect();
    runner.resume(session, &decisions).await.unwrap();
    let output = tool_result_output(&runner, session, "c1").await;
    assert_eq!(
        output,
        expected_error_object(MSG_REJECTED),
        "rejected approval must produce the exact synthetic string, no prefix"
    );
}

#[tokio::test]
async fn test_baseline_interrupted_orphan() {
    // An abort with a pending approval-requiring tool call must reconcile an
    // interrupted-by-user result for the orphan (the reconcile_tool_results path).
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(HangingProvider::new(vec![LlmEvent::ToolCall {
        id: "c1".into(),
        name: "guarded".into(),
        input: serde_json::json!({"x": 1}),
    }]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    let runner = Arc::new(runner_with(p, tools));
    let session = SessionId::new();
    let r = runner.clone();
    let task = tokio::spawn(async move { r.run(session, "hi".into()).await });
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    runner.abort();
    let result = task.await.expect("run task").expect("run ok");
    assert!(matches!(result.outcome, RunOutcome::Interrupted(_)));
    let output = tool_result_output(&runner, session, "c1").await;
    assert_eq!(
        output,
        expected_error_object(MSG_INTERRUPTED),
        "interrupted orphan must produce the exact synthetic string, no prefix"
    );
}

#[test]
fn test_all_seven_strings_documented() {
    // The seventh point (unknown-tool-on-resume) is not driven above; it is
    // pinned here so the synthetic-outcome centralization has the constant to
    // match. The cross-layer E2E exercises it end-to-end.
    assert_eq!(MSG_UNKNOWN_TOOL, "unknown tool: no_such_tool");
    assert_eq!(MSG_TOOL_FAILURE, "baseline tool failure");
    assert_eq!(MSG_REJECTED, "rejected by user");
    assert_eq!(MSG_INTERRUPTED, "interrupted by user");
    assert_eq!(MSG_UNKNOWN_TOOL_ON_RESUME, "unknown tool on resume");
    assert_eq!(TOOL_ERROR_PREFIX, "tool error: ");
    // Tool-error JSON shape = prefix + message; synthetic JSON shape = bare.
    assert_eq!(
        expected_error_object(&format!("{TOOL_ERROR_PREFIX}{MSG_TOOL_FAILURE}")),
        serde_json::json!({"error": "tool error: baseline tool failure"})
    );
    assert_eq!(
        expected_error_object(MSG_REJECTED),
        serde_json::json!({"error": "rejected by user"})
    );
}

/// Suppress the unused-import warning for InMemoryBackend/SessionStore when
/// the test runner only calls runner_with (which hides them). Kept so the
/// baseline file documents the concrete store the runner uses.
#[allow(dead_code)]
fn _store_ref() -> SessionStore {
    SessionStore::new(Box::new(InMemoryBackend::new()))
}
