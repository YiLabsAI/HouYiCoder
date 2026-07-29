//! Integration test: the agent loop wires BashTool + a real macOS Seatbelt
//! sandbox + HITL approval end to end. Exercises Runner run/resume against
//! MacSeatbeltSession (real sandbox-exec), so it lives outside the unit suite.
//! Run with: make test-integration. Mac-only.

#![cfg(target_os = "macos")]

use std::sync::Arc;

use houyicoder_api::provider::ModelProvider;
use houyicoder_async::PFut;
use houyicoder_context::SessionId;
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::Usage;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, ModelCapabilities, OutputItem, ProviderError,
};
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{ApprovalDecision, BashTool, RunOutcome, Runner, ToolRegistry};
use houyicoder_sandbox::MacSeatbeltSession;

/// A provider that returns scripted responses in sequence, then repeats the
/// last one forever. Test-only helper re-implemented here because integration
/// tests reach the crate only through its public API.
struct FakeProvider {
    responses: std::sync::Mutex<Option<std::vec::IntoIter<CompletionResponse>>>,
    last: std::sync::Mutex<Option<CompletionResponse>>,
}

impl FakeProvider {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(Some(responses.into_iter())),
            last: std::sync::Mutex::new(None),
        }
    }
}

impl ModelProvider for FakeProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let next = {
            let mut iter_guard = self.responses.lock().expect("script mutex");
            if let Some(iter) = iter_guard.as_mut() {
                if let Some(next) = iter.next() {
                    *self.last.lock().expect("last mutex") = Some(next.clone());
                    Some(next)
                } else {
                    self.last.lock().expect("last mutex").clone()
                }
            } else {
                None
            }
        };
        let next = next.expect("script has no responses");
        Box::pin(async move { Ok(next) })
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    fn stream(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> houyicoder_async::PStream<
        '_,
        Result<houyicoder_protocol::llm::LlmEvent, houyicoder_protocol::llm::ProviderError>,
    > {
        let next = {
            let mut iter_guard = self.responses.lock().expect("script mutex");
            if let Some(iter) = iter_guard.as_mut() {
                if let Some(next) = iter.next() {
                    *self.last.lock().expect("last mutex") = Some(next.clone());
                    Some(next)
                } else {
                    self.last.lock().expect("last mutex").clone()
                }
            } else {
                None
            }
        };
        houyicoder_api::provider::stream_from_response(next.expect("script has no responses"))
    }
}

#[tokio::test]
async fn test_loop_bash() {
    let responses = vec![
        CompletionResponse {
            output: vec![
                OutputItem::Text {
                    text: "let me run".into(),
                },
                OutputItem::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo integrated-loop > log.txt"}),
                },
            ],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    let provider = Arc::new(FakeProvider::new(responses));
    let session: Arc<dyn SandboxSession> = Arc::new(MacSeatbeltSession::new().expect("seatbelt"));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BashTool::new(session.clone())));
    let runner = Runner::new(
        std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry {
                max_attempts: 2,
                ..Retry::default()
            },
        },
    );
    let sid = SessionId::new();

    let result = runner.run(sid, "run bash".into()).await.expect("run");
    let approvals = match result.outcome {
        RunOutcome::Interruption(a) => a,
        other => panic!("expected interruption, got {other:?}"),
    };
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0].tool_name, "bash");
    let decisions = approvals
        .iter()
        .map(|a| ApprovalDecision::approve(&a.call_id))
        .collect::<Vec<_>>();

    let resumed = runner.resume(sid, &decisions).await.expect("resume");
    match resumed.outcome {
        RunOutcome::FinalOutput(t) => assert_eq!(t, "done"),
        other => panic!("expected final output, got {other:?}"),
    }
    // The sandbox really ran the command: log.txt exists with our text.
    let log =
        String::from_utf8(session.read_file("log.txt", 64).await.expect("read")).expect("utf8");
    assert_eq!(log.trim(), "integrated-loop");
}
