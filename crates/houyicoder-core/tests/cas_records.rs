//! A real runner turn records what its request carried from the block store.
//!
//! The retention path chooses the form to carry while assembling the model input,
//! the runner reports it to the session's view record, and the run resolve
//! writes it to the durable log. This drives all four stages through a real
//! Runner, a real SessionStore, and a stub provider, so a record that never
//! reaches the log fails here rather than showing up as a missing figure in
//! the context view.

use std::sync::Arc;

use houyicoder_api::provider::ModelProvider;
use houyicoder_api::provider::stream_from_response;
use houyicoder_api::session::SessionLog;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry, ViewForm};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::block_ref::BLOCK_REF_KEY;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;
use serde_json::json;

use houyicoder_core::agent::StubTool;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{RunOutcome, Runner, ToolRegistry};

/// A provider that answers every call with one line of text and no tools.
struct TextProvider;

impl ModelProvider for TextProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async move { Ok(response()) })
    }
    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        stream_from_response(response())
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

fn response() -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".to_string(),
        }],
        usage: Usage::default(),
        model: "test".to_string(),
    }
}

fn entry(session: SessionId, event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 1,
        prev_hash: None,
        event,
    }
}

fn config() -> RunnerConfig {
    RunnerConfig {
        model: "test".into(),
        instructions: String::new(),
        max_turns: 2,
        max_output_tokens: 8_000,
        retry: Retry {
            max_attempts: 2,
            ..Retry::default()
        },
    }
}

/// One turn over a stored block reports the form its request carried, in the
/// unit the assembly counted, so a later replay states what was sent without
/// re-running the retention decision.
#[tokio::test]
async fn test_turn_logs_model_view() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let log: Arc<dyn SessionLog> = store.clone();
    let session = SessionId::new();
    let block = store
        .backend()
        .block_put(
            serde_json::to_vec(&json!({"stdout": "x".repeat(4096)}))
                .expect("the stored output serializes"),
        )
        .await
        .expect("the block is stored")
        .0;
    log.append(entry(
        session,
        SessionEvent::ToolCall {
            call_id: "call-1".to_string(),
            tool: "bash".to_string(),
            input: json!({"command": "print"}),
        },
    ))
    .await
    .expect("the call is logged");
    log.append(entry(
        session,
        SessionEvent::ToolResult {
            call_id: "call-1".to_string(),
            output: json!({BLOCK_REF_KEY: block}),
            duration_ms: 3,
        },
    ))
    .await
    .expect("the result is logged");

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(StubTool::new("bash")));
    let runner = Runner::new(log.clone(), Arc::new(TextProvider), tools, config());
    let outcome = runner
        .run(session, "hi".into())
        .await
        .expect("the run ends");
    assert!(matches!(outcome.outcome, RunOutcome::FinalOutput(_)));

    let events = log.replay(session).await.expect("the log replays");
    let reported: Vec<_> = events
        .iter()
        .filter_map(|entry| match &entry.event {
            SessionEvent::CasModelView {
                views,
                estimate_unit,
                ..
            } => Some((views, estimate_unit)),
            _ => None,
        })
        .collect();
    assert_eq!(reported.len(), 1, "one assembly is reported");
    let (views, unit) = reported[0];
    assert_eq!(views.len(), 1, "the one block the request carried");
    assert_eq!(views[0].block_ref, block);
    assert_eq!(
        views[0].form,
        ViewForm::Materialize,
        "the block was reachable, so its content was carried"
    );
    assert!(views[0].size > 0, "the carried form was measured");
    assert_eq!(unit, "tokens", "and measured in the assembly's unit");
}
