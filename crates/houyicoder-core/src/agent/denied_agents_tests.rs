//! Dispatch-threading tests for the per-call ToolCtx: the denied-agent set,
//! the spawn port, and the agent identity the composition root threads onto
//! the runner all reach the tool at dispatch time.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use houyicoder_api::spawn::{AgentIdentity, SpawnArgs, SpawnFailure, SpawnHandle, SpawnOutcome};
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::SessionId;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};

use crate::agent::tests::runner_with;
use crate::agent::{RunOutcome, ToolRegistry};
use crate::provider::test_support::FakeProvider;

/// A tool that captures the per-call context it saw, then returns a plain
/// result so the run finishes on the next scripted response. The
/// concurrency_safe flag drives which dispatch path (parallel batch vs
/// serial) the tool takes, so both paths can be exercised.
struct DenyCaptureTool {
    seen: Arc<Mutex<Option<CapturedCtx>>>,
    concurrency_safe: bool,
}

#[derive(Clone)]
struct CapturedCtx {
    denied: HashSet<String>,
    spawn_handle_present: bool,
    identity_depth: u32,
}

impl Tool for DenyCaptureTool {
    fn name(&self) -> &str {
        "denycap"
    }
    fn description(&self) -> &str {
        "captures the per-call context from its ToolCtx"
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
            *seen.lock().unwrap() = Some(CapturedCtx {
                denied: ctx.denied_agents.as_ref().clone(),
                spawn_handle_present: ctx.spawn_handle.is_some(),
                identity_depth: ctx.agent_identity.as_ref().map(|i| i.depth).unwrap_or(0),
            });
            Ok(serde_json::json!({"ok": true}))
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        self.concurrency_safe
    }
    fn is_destructive(&self) -> bool {
        false
    }
}

/// A spawn handle that rejects every spawn, so a dispatch carrying it can
/// assert presence without launching a child.
struct NoSpawn;
impl SpawnHandle for NoSpawn {
    fn spawn(
        &self,
        _ctx: &ToolCtx,
        _args: SpawnArgs,
    ) -> PFut<'_, Result<SpawnOutcome, SpawnFailure>> {
        Box::pin(async { Err(SpawnFailure::Recursive) })
    }
}

fn two_response_script() -> Arc<dyn houyicoder_api::provider::ModelProvider> {
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
    Arc::new(FakeProvider::new(vec![first, second]))
}

#[tokio::test]
async fn test_dispatch_threads_denied_agents() {
    let seen = Arc::new(Mutex::new(None));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(DenyCaptureTool {
        seen: seen.clone(),
        concurrency_safe: true,
    }));
    let denied: Arc<HashSet<String>> = Arc::new(["explore".to_string()].into_iter().collect());
    let runner = runner_with(two_response_script(), tools).with_denied_agents(denied);
    let session = SessionId::new();
    let result = runner.run(session, "go".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    let got = seen.lock().unwrap().clone().expect("tool must have run");
    assert!(got.denied.contains("explore"), "got {:?}", got.denied);
    // No spawn handle wired on a plain test runner.
    assert!(!got.spawn_handle_present);
}

#[tokio::test]
async fn test_dispatch_threads_spawn_identity() {
    let seen = Arc::new(Mutex::new(None));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(DenyCaptureTool {
        seen: seen.clone(),
        concurrency_safe: true,
    }));
    let identity = AgentIdentity {
        subagent_type: Some("explore".into()),
        depth: 2,
        parent_session_id: None,
    };
    let runner = runner_with(two_response_script(), tools)
        .with_spawn_handle(Arc::new(NoSpawn) as Arc<dyn SpawnHandle>)
        .with_agent_identity(identity);
    let session = SessionId::new();
    let result = runner.run(session, "go".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    let got = seen.lock().unwrap().clone().expect("tool must have run");
    assert!(got.spawn_handle_present, "spawn handle must reach the tool");
    assert_eq!(got.identity_depth, 2, "agent identity depth must thread");
}

#[tokio::test]
async fn test_dispatch_default_denied_empty() {
    let seen = Arc::new(Mutex::new(None));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(DenyCaptureTool {
        seen: seen.clone(),
        concurrency_safe: true,
    }));
    let runner = runner_with(two_response_script(), tools);
    let session = SessionId::new();
    let result = runner.run(session, "go".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    let got = seen.lock().unwrap().clone().expect("tool must have run");
    assert!(got.denied.is_empty(), "got {:?}", got.denied);
}

#[tokio::test]
async fn test_dispatch_serial_spawn() {
    // A non-concurrency-safe tool takes the serial dispatch path, so the
    // spawn-handle threading there (not just the parallel-batch branch)
    // must also reach the tool.
    let seen = Arc::new(Mutex::new(None));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(DenyCaptureTool {
        seen: seen.clone(),
        concurrency_safe: false,
    }));
    let runner = runner_with(two_response_script(), tools)
        .with_spawn_handle(Arc::new(NoSpawn) as Arc<dyn SpawnHandle>);
    let session = SessionId::new();
    let result = runner.run(session, "go".into()).await.expect("run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    let got = seen.lock().unwrap().clone().expect("tool must have run");
    assert!(
        got.spawn_handle_present,
        "spawn handle must reach the tool on the serial path",
    );
}
