//! Budget-pressure gate: capture real CompletionRequests and assert the
//! runtime never injects a user item that urges the model to stop early.
//!
//! A fabricated user turn telling the model its budget is nearly gone is not
//! a neutral reminder: a reasoning model reads it as the user asking for a
//! partial answer now, abandons the remaining work, and ends the run with a
//! plain stop, which also loses the turn-limit outcome the terminal reports.

use std::sync::{Arc, Mutex};

use houyicoder_api::provider::ModelProvider;
use houyicoder_api::provider::stream_from_response;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::SessionId;
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, InputItem, ModelCapabilities, OutputItem, ProviderError,
};
use houyicoder_protocol::llm::{LlmEvent, Usage};
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;

use houyicoder_core::agent::StubTool;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use std::vec::IntoIter;

struct ScriptCapturingProvider {
    responses: Mutex<IntoIter<CompletionResponse>>,
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl ScriptCapturingProvider {
    fn new(responses: Vec<CompletionResponse>, seen: Arc<Mutex<Vec<CompletionRequest>>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter()),
            seen,
        }
    }
}

impl ModelProvider for ScriptCapturingProvider {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.seen.lock().expect("mutex").push(req);
        let next = self.responses.lock().expect("mutex").next();
        Box::pin(async move { Ok(next.expect("script has enough responses")) })
    }
    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        self.seen.lock().expect("mutex").push(req);
        let next = self
            .responses
            .lock()
            .expect("mutex")
            .next()
            .expect("script has enough responses");
        stream_from_response(next)
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

#[tokio::test]
async fn test_injected_messages_no_pressure() {
    // 5 turns (max_turns=5). Responses 0-3 carry a tool call (loop
    // continues); response 4 is text (FinalOutput). The convergence
    // reminder fires on turns 2+ (turn > 1 && remaining <= 5). Captured
    // User items on those turns must not contain pressure patterns.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut responses = Vec::new();
    for _ in 0..4 {
        responses.push(CompletionResponse {
            output: vec![
                OutputItem::Text {
                    text: "checking".into(),
                },
                OutputItem::ToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"x": 1}),
                },
            ],
            usage: Usage::default(),
            model: "test".into(),
        });
    }
    responses.push(CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    });

    let provider = Arc::new(ScriptCapturingProvider::new(responses, seen.clone()));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(StubTool::new("echo")));
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
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
    let session = SessionId::new();
    let _result = runner
        .run(session, "do some work".into())
        .await
        .expect("run");

    let captured = seen.lock().expect("mutex").clone();
    assert!(
        captured.len() >= 3,
        "the run must make several calls for this to mean anything, got {}",
        captured.len()
    );

    // Nothing the runtime injects may urge the model to stop early. A fake
    // user turn saying the budget is nearly gone reads to a reasoning model
    // as the user asking it to wrap up, so it abandons the remaining work and
    // returns a partial answer, and the run then ends in a plain stop instead
    // of the turn-limit outcome the terminal reports.
    const PRESSURE_PATTERNS: [&str; 6] = [
        "gathered enough",
        "synthesize",
        "final answer",
        "remaining turns",
        "budget",
        "wrap up",
    ];
    for (i, request) in captured.iter().enumerate() {
        for item in &request.input {
            let InputItem::User { content } = item else {
                continue;
            };
            let lower = content.to_ascii_lowercase();
            for pattern in PRESSURE_PATTERNS {
                assert!(
                    !lower.contains(&pattern.to_ascii_lowercase()),
                    "call {} carries the pressure phrase {pattern:?} in a user \
                     item: {content}",
                    i + 1
                );
            }
        }
    }

    // The loop must run to its own end rather than being talked into stopping:
    // every scripted tool call is consumed, so all five responses were used.
    assert_eq!(
        captured.len(),
        5,
        "the run used every scripted response instead of stopping early"
    );
}
