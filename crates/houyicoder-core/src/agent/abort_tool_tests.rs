//! Extracted abort-during-tool-execution test. Kept in a separate file so
//! loop_tests.rs stays under the file-size gate.

use super::*;
use crate::agent::tests::runner_with;
use crate::provider::test_support::FakeProvider;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_context::TurnEventKind;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};

/// A concurrency-safe tool whose execute future never resolves. Proves the
/// run CancellationToken propagates into tool dispatch: without the
/// select!{cancelled => ...} race in execute_partitioned, a bare .await on
/// this pending future would hang the run forever on abort. With the race,
/// abort fires the token, the partition select! short-circuits to an
/// interrupted result, and the run resolves Interrupted.
struct HangingTool {
    name: String,
}
impl HangingTool {
    fn new() -> Self {
        Self {
            name: "hanging".into(),
        }
    }
}
impl Tool for HangingTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "a tool that never returns (its execute future is pending forever)"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        _input: serde_json::Value,
    ) -> houyicoder_async::PFut<'_, Result<serde_json::Value, ToolError>> {
        Box::pin(async move {
            // Never resolves: the only way out is the partition-level
            // select!{cancelled => interrupted} race on abort.
            std::future::pending::<()>().await;
            Ok(serde_json::json!({"unreachable": true}))
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
}

/// Abort during TOOL EXECUTION (not the model stream) must resolve the run.
/// The CancellationToken propagates through resolve_turn -> execute_partitioned
/// into the partition select! that races each tool future against
/// cancellation. A HangingTool (pending execute future) would hang the run
/// forever on a bare .await; the select! short-circuits to an interrupted
/// result on abort.
#[tokio::test]
async fn test_abort_in_tool_dispatch() {
    let resp = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "hanging".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![resp]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HangingTool::new()));
    let runner = Arc::new(runner_with(p, tools));
    let session = SessionId::new();
    let r = runner.clone();
    let task = tokio::spawn(async move { r.run(session, "hi".into()).await });
    // Let the run enter tool dispatch (model_call_stream returns the tool
    // call, resolve_turn reaches execute_partitioned, the HangingTool future
    // is polled and goes pending).
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    runner.abort();
    // Without the select!{cancelled} race this await would hang forever (the
    // pending tool future never returns). A timeout makes the discrimination
    // explicit: the fix resolves promptly, the regression hangs.
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("run resolved on abort within 2s (token reaches tool dispatch)")
        .expect("run task")
        .expect("run ok");
    assert!(
        matches!(result.outcome, RunOutcome::Interrupted(_)),
        "expected Interrupted, got {:?}",
        result.outcome
    );
    let events = runner.store().replay(session).await.expect("replay");
    // An interrupted tool result lands so the session stays lossless.
    let interrupted = events
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::ToolResult { .. }))
        .count();
    assert!(
        interrupted >= 1,
        "an interrupted tool result lands for the hanging call"
    );
}

/// A concurrency-safe tool that completes at once. Exercises the
/// per-completion arm of the parallel exec batch (the group.next() -> Some
/// path), which HangingTool never reaches because its execute future never
/// resolves.
struct SafeProbeTool;
impl SafeProbeTool {
    fn new() -> Self {
        Self
    }
}
impl Tool for SafeProbeTool {
    fn name(&self) -> &str {
        "probe"
    }
    fn description(&self) -> &str {
        "a concurrency-safe tool that returns at once"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        input: serde_json::Value,
    ) -> houyicoder_async::PFut<'_, Result<serde_json::Value, ToolError>> {
        Box::pin(async move { Ok(serde_json::json!({"ran": input})) })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn test_parallel_batch_streams() {
    // Two concurrency-safe calls in one turn both complete; each result
    // appends as the call finishes (the per-completion arm), and the run
    // continues to the next turn's final text. Guards against the old
    // join_all barrier, which held every result until the slowest call
    // returned — the live delta dumped the batch at once instead of
    // streaming each result as it landed.
    let resp1 = CompletionResponse {
        output: vec![
            OutputItem::ToolCall {
                id: "c1".into(),
                name: "probe".into(),
                input: serde_json::json!({"n": 1}),
            },
            OutputItem::ToolCall {
                id: "c2".into(),
                name: "probe".into(),
                input: serde_json::json!({"n": 2}),
            },
        ],
        usage: Usage::default(),
        model: "test".into(),
    };
    let resp2 = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![resp1, resp2]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SafeProbeTool::new()));
    let runner = runner_with(p, tools);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::FinalOutput(_)),
        "run reached the final-text turn after both probes completed"
    );
    let events = runner.store().replay(session).await.unwrap();
    let has_result = |cid: &str| {
        events
            .iter()
            .any(|e| matches!(&e.kind, TurnEventKind::ToolResult { call_id, .. } if call_id == cid))
    };
    assert!(
        has_result("c1") && has_result("c2"),
        "both parallel calls landed results"
    );
}
