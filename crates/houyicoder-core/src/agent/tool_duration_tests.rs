//! Per-call duration timing tests for the durable ToolResult event.
//!
//! The main execute_partitioned path (parallel-safe batch + serial-unsafe)
//! measures each call's wall-clock length and writes it to
//! ToolResult.duration_ms. Synthetic results (interrupted / unknown tool)
//! carry 0 since no real execution ran. These pin both contracts so the
//! trajectory's latency dimension stays honest on resume + export.

use std::sync::Arc;
use std::time::Duration;

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_context::{SessionId, TurnEventKind};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use serde_json::Value;

use super::tests::runner_with;
use super::*;
use crate::provider::test_support::FakeProvider;

/// A tool that sleeps a fixed duration before returning, so the per-call
/// timing has a known lower bound. The safe flag picks the parallel vs serial
/// branch.
struct SleepingTool {
    name: String,
    safe: bool,
    sleep_ms: u64,
}

impl SleepingTool {
    fn new(name: &str, safe: bool, sleep_ms: u64) -> Self {
        Self {
            name: name.to_string(),
            safe,
            sleep_ms,
        }
    }
}

impl Tool for SleepingTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "sleeps then returns ok, for duration timing"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        _input: Value,
    ) -> houyicoder_async::PFut<'_, Result<Value, ToolError>> {
        let ms = self.sleep_ms;
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(serde_json::json!({"ok": true}))
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        self.safe
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
}

fn tool_call_response(tool: &str) -> CompletionResponse {
    CompletionResponse {
        output: vec![
            OutputItem::Text {
                text: format!("run {tool}"),
            },
            OutputItem::ToolCall {
                id: "c1".into(),
                name: tool.into(),
                input: serde_json::json!({}),
            },
        ],
        usage: Usage::default(),
        model: "test".into(),
    }
}

fn final_response() -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    }
}

/// Look up the duration_ms recorded for a call_id in the session log.
async fn duration_for(runner: &Runner, session: SessionId, call_id: &str) -> u64 {
    let events = runner.store().replay(session).await.expect("replay");
    for ev in &events {
        if let TurnEventKind::ToolResult {
            call_id: cid,
            duration_ms,
            ..
        } = &ev.kind
            && cid == call_id
        {
            return *duration_ms;
        }
    }
    panic!("no ToolResult for {call_id}");
}

#[tokio::test]
async fn test_parallel_safe_call_records() {
    // A 12ms sleep => elapsed is always >= 12ms (sleep guarantees at least
    // its duration), so as_millis >= 12. Assert >= 8 for timer-granularity
    // margin. Lower bound only — no upper bound (CI load varies).
    let p = Arc::new(FakeProvider::new(vec![
        tool_call_response("sleeper"),
        final_response(),
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SleepingTool::new("sleeper", true, 12)));
    let runner = runner_with(p, tools);
    let session = SessionId::new();
    runner.run(session, "hi".into()).await.unwrap();
    let d = duration_for(&runner, session, "c1").await;
    assert!(
        d >= 8,
        "parallel-safe call must record real duration, got {d}ms"
    );
}

#[tokio::test]
async fn test_serial_unsafe_call_records() {
    // Non-safe tool => serial branch. Same lower-bound logic as the parallel
    // test; pins the serial timing site independently.
    let p = Arc::new(FakeProvider::new(vec![
        tool_call_response("sleeper"),
        final_response(),
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SleepingTool::new("sleeper", false, 12)));
    let runner = runner_with(p, tools);
    let session = SessionId::new();
    runner.run(session, "hi".into()).await.unwrap();
    let d = duration_for(&runner, session, "c1").await;
    assert!(
        d >= 8,
        "serial-unsafe call must record real duration, got {d}ms"
    );
}

#[tokio::test]
async fn test_unknown_tool_zero_duration() {
    // A call to a tool not in the registry lands a synthetic unknown-tool
    // result with no execution => duration_ms stays 0. Pins the synthetic
    // contract so a future regression that wires timing into the synthetic
    // branch does not silently inflate interrupted/blocked latencies.
    let p = Arc::new(FakeProvider::new(vec![
        tool_call_response("no_such_tool"),
        final_response(),
    ]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    runner.run(session, "hi".into()).await.unwrap();
    let d = duration_for(&runner, session, "c1").await;
    assert_eq!(d, 0, "synthetic result must carry 0 duration, got {d}ms");
}
