//! Dispatch-threading test for the denied-agent set: the runner carries the
//! set from the composition root through the per-call ToolCtx so the agent
//! tool can tell a denial from an unknown type at resolve time.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::SessionId;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};

use crate::agent::tests::runner_with;
use crate::agent::{RunOutcome, ToolRegistry};
use crate::provider::test_support::FakeProvider;

/// Captures the denied-agent set its dispatch carried, then returns a plain
/// result so the run finishes on the next scripted response.
struct DenyCaptureTool {
    seen: Arc<Mutex<Option<HashSet<String>>>>,
}

impl Tool for DenyCaptureTool {
    fn name(&self) -> &str {
        "denycap"
    }
    fn description(&self) -> &str {
        "captures the denied-agent set from its ToolCtx"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(
        &self,
        ctx: ToolCtx,
        _input: serde_json::Value,
    ) -> PFut<'_, Result<serde_json::Value, ToolError>> {
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            *seen.lock().unwrap() = Some(ctx.denied_agents.as_ref().clone());
            Ok(serde_json::json!({"ok": true}))
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn test_dispatch_threads_denied_agents() {
    let seen = Arc::new(Mutex::new(None));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(DenyCaptureTool { seen: seen.clone() }));
    let first = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "call_1".into(),
            name: "denycap".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let second = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::new(vec![first, second]));
    let denied: Arc<HashSet<String>> = Arc::new(["explore".to_string()].into_iter().collect());
    let runner = runner_with(provider, tools).with_denied_agents(denied);
    let session = SessionId::new();
    let result = runner.run(session, "go".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    let got = seen.lock().unwrap().clone().expect("tool must have run");
    assert!(got.contains("explore"), "got {got:?}");
}

#[tokio::test]
async fn test_dispatch_default_denied_empty() {
    // A runner built without a denied set dispatches with an empty set, not
    // a missing one, so a tool that reads it never branches on None.
    let seen = Arc::new(Mutex::new(None));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(DenyCaptureTool { seen: seen.clone() }));
    let first = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "call_1".into(),
            name: "denycap".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let second = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::new(vec![first, second]));
    let runner = runner_with(provider, tools);
    let session = SessionId::new();
    let result = runner.run(session, "go".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    let got = seen.lock().unwrap().clone().expect("tool must have run");
    assert!(got.is_empty(), "got {got:?}");
}
