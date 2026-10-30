//! A tool call's arguments are model output, so the stream of fragments that
//! writes them reaches the live-response observers. Without that a reader
//! keeping its own clock sees silence for the whole time the model spends
//! writing a large call, and reports a stall on a turn that is going as
//! expected.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::stream::iter;
use houyicoder_api::agent_event::{AgentEventHandlers, ResponseStreamEvent};
use houyicoder_api::provider::ModelProvider;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::SessionId;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, ProviderError,
};

use super::runner_with;
use crate::agent::ToolRegistry;
use crate::provider::test_support::FakeProvider;

/// A provider that writes one tool call the way the wire does: the arguments
/// arrive as fragments, then the call itself.
struct StreamingCallProvider {
    inner: Arc<FakeProvider>,
    calls: AtomicU32,
}

impl ModelProvider for StreamingCallProvider {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.inner.complete(req)
    }

    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let items = vec![
                Ok(LlmEvent::ToolInputStart {
                    id: "c1".into(),
                    name: "write".into(),
                }),
                Ok(LlmEvent::ToolInputDelta {
                    id: "c1".into(),
                    name: "write".into(),
                    text: "{\"path\":".into(),
                }),
                Ok(LlmEvent::ToolInputDelta {
                    id: "c1".into(),
                    name: "write".into(),
                    text: "\"a.rs\"}".into(),
                }),
                Ok(LlmEvent::ToolInputEnd {
                    id: "c1".into(),
                    name: "write".into(),
                }),
                Ok(LlmEvent::ToolCall {
                    id: "c1".into(),
                    name: "done".into(),
                    input: serde_json::json!({}),
                }),
                Ok(LlmEvent::Finish {
                    reason: "tool_calls".into(),
                    usage: None,
                }),
            ];
            Box::pin(iter(items))
        } else {
            self.inner.stream(req)
        }
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// The fragments a call is written with reach the live-response observers, in
/// order, so a reader can tell a writing model from a stalled one.
#[tokio::test]
async fn test_tool_input_reaches_observers() {
    let provider: Arc<dyn ModelProvider> = Arc::new(StreamingCallProvider {
        inner: Arc::new(FakeProvider::text("done")),
        calls: AtomicU32::new(0),
    });
    let mut runner = runner_with(provider, ToolRegistry::new());
    let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&collected);
    let mut events = AgentEventHandlers::default();
    events.set_response_stream(Arc::new(move |event: ResponseStreamEvent| {
        if let ResponseStreamEvent::ToolInputDelta { text } = event {
            sink.lock().expect("events").push(text);
        }
    }));
    runner.set_event_handlers(events);
    let result = runner.run(SessionId::new(), "write a file".into()).await;
    assert!(result.is_ok(), "the scripted run finishes: {result:?}");

    let fragments = collected.lock().expect("events").clone();
    assert_eq!(
        fragments,
        vec!["{\"path\":".to_string(), "\"a.rs\"}".to_string()],
        "every argument fragment reaches the observer, in order"
    );
}
