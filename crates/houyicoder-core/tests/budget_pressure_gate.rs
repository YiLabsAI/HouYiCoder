//! Budget-pressure gate: capture real CompletionRequests and assert the
//! runtime-injected User items are constant across turns — any per-turn
//! variation = budget interpolation.

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

struct ScriptCapturingProvider {
    responses: Mutex<std::vec::IntoIter<CompletionResponse>>,
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
        "at least 3 requests (reminder fires turn 2+), got {}",
        captured.len()
    );

    // Constancy gate: the trailing User item per request (houyi-injected)
    // must be constant across turns. Any per-turn variation = budget
    // interpolation (turn count, %, remaining).
    //
    // Skip the first trailing item (turn 0's is the real user prompt).
    let trailing: Vec<String> = captured
        .iter()
        .filter_map(|r| match r.input.last() {
            Some(InputItem::User { content }) => Some(content.clone()),
            _ => None,
        })
        .skip(1)
        .collect();
    if let Some(first) = trailing.first() {
        for (i, t) in trailing.iter().enumerate() {
            assert_eq!(
                first,
                t,
                "trailing User item varies across turns (turn {}+) = \
                 per-turn state interpolated into model input",
                i + 1
            );
        }
    }
}
