//! Provider adapters: FakeProvider and OpenAiCompatibleProvider.
//! The ModelProvider trait and the LLM data types live elsewhere
//! (ports::provider and protocol::llm); this crate holds the adapter
//! impls that satisfy the port.

use houyicoder_api::provider::ModelProvider;
use houyicoder_async::PFut;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};

mod http_error;
mod openai_compat;
mod served_models;

/// Adapt a complete (non-streaming) response into a stream of LlmEvents, so a
/// provider that only knows how to produce a CompletionResponse can still satisfy
/// the stream() seam — and so tests with scripted responses plug into the
/// streaming loop without re-implementing the event taxonomy. Text is chunked
/// into 4-char deltas to mimic token-by-token delivery; tool calls and reasoning
/// are emitted as single events. Re-exported from ports so adapter crates and
/// consumers share one copy.
pub use houyicoder_api::provider::stream_from_response;
pub use openai_compat::OpenAiCompatibleProvider;

/// Deterministic fake provider (no LLM): returns a programmed sequence of
/// canned responses, repeating the last forever. A single-element sequence is
/// a stateless double that always returns that response. Lets the agent loop
/// and the UI tests run without a model dependency; a real provider plugs in
/// behind the same trait in production. Following the test-double taxonomy
/// (Meszaros), this is a fake — a working, simplified in-memory implementation
/// — matching the LLM-tooling convention (LangChain FakeListChatModel).
#[derive(Debug)]
pub struct FakeProvider {
    responses: Vec<CompletionResponse>,
    call: std::sync::atomic::AtomicU32,
    /// Per-instance delay before a stream's first event (ms). Test affordance
    /// to let out-of-band wire messages (e.g. InjectUser from the host's
    /// run-end batch) land in the server's input_queue before the first model
    /// call returns, so the turn-boundary race is deterministic, not
    /// timing-luck.
    delay_ms: Option<u64>,
    /// When set, every complete() call returns this error instead of a
    /// response. Test affordance for error-classification tests (inject
    /// 401 Auth / 404 ModelNotFound without a real HTTP server).
    error: Option<ProviderError>,
}

impl FakeProvider {
    /// Build from an ordered list of responses. The last is repeated for any
    /// model calls past the list length, so a run that loops past the sequence
    /// ends on the final response instead of re-emitting an earlier one. An
    /// empty list is allowed for placeholder providers that are never called;
    /// a call against an empty list panics (fail loudly on use, not on
    /// construction).
    pub fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses,
            call: std::sync::atomic::AtomicU32::new(0),
            delay_ms: None,
            error: None,
        }
    }

    /// Like new, but sleeps delay_ms before a stream's first event. A test
    /// affordance to make the turn-boundary race deterministic (an InjectUser
    /// sent right after SendMessage lands in the input_queue before the first
    /// model call returns, so the drive_loop turn-boundary drain consumes
    /// it -- the race-win path).
    pub fn new_with_delay(responses: Vec<CompletionResponse>, delay_ms: u64) -> Self {
        Self {
            responses,
            call: std::sync::atomic::AtomicU32::new(0),
            delay_ms: Some(delay_ms),
            error: None,
        }
    }

    /// Build a provider that always errors with the given ProviderError.
    /// Test affordance for error-classification tests (inject 401 Auth /
    /// 404 ModelNotFound without a real HTTP server).
    pub fn with_error(err: ProviderError) -> Self {
        Self {
            responses: Vec::new(),
            call: std::sync::atomic::AtomicU32::new(0),
            delay_ms: None,
            error: Some(err),
        }
    }

    /// Convenience: a single text response (a one-element sequence that always
    /// returns that text).
    pub fn text(text: &str) -> Self {
        Self::new(vec![CompletionResponse {
            output: vec![OutputItem::Text {
                text: text.to_string(),
            }],
            usage: Usage::default(),
            model: "test".to_string(),
        }])
    }

    /// Build from a list of output-item lists (one per response). Each inner
    /// list becomes one CompletionResponse with default usage and a stub model
    /// tag. Convenient for tests authoring a sequence inline.
    pub fn from_outputs(per_call: Vec<Vec<OutputItem>>) -> Self {
        let responses = per_call
            .into_iter()
            .map(|output| CompletionResponse {
                output,
                usage: Usage::default(),
                model: "test".to_string(),
            })
            .collect();
        Self::new(responses)
    }

    fn next_response(&self) -> CompletionResponse {
        use std::sync::atomic::Ordering;
        if self.responses.is_empty() {
            panic!("FakeProvider called with no responses queued");
        }
        let i = self.call.fetch_add(1, Ordering::SeqCst);
        let idx = (i as usize).min(self.responses.len() - 1);
        self.responses[idx].clone()
    }
}

impl ModelProvider for FakeProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        if let Some(err) = self.error.clone() {
            return Box::pin(async move { Err(err) });
        }
        let resp = self.next_response();
        Box::pin(async move { Ok(resp) })
    }

    fn stream(
        &self,
        _req: CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
        let resp = self.next_response();
        let delay = self.delay_ms;
        match delay {
            Some(ms) => Box::pin(async_stream::stream! {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                use futures::StreamExt;
                let mut inner = stream_from_response(resp);
                while let Some(ev) = inner.next().await {
                    yield ev;
                }
            }),
            None => stream_from_response(resp),
        }
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_protocol::llm::{CompletionRequest, InputItem, ModelSettings, ToolDef};

    #[test]
    fn test_fake_returns_canned_text() {
        let p = FakeProvider::text("hello");
        let req = CompletionRequest {
            model: "test".into(),
            instructions: "you are a test".into(),
            input: vec![InputItem::User {
                content: "hi".into(),
            }],
            tools: vec![],
            settings: ModelSettings::default(),
            cache_breakpoints: Vec::new(),
        };
        let resp = pollster::block_on(p.complete(req)).expect("fake completes");
        assert_eq!(resp.output.len(), 1);
        assert!(matches!(resp.output[0], OutputItem::Text { ref text } if text == "hello"));
        assert!(!resp.has_tool_calls());
    }

    #[test]
    fn test_has_tool_calls_detects() {
        let resp = CompletionResponse {
            output: vec![
                OutputItem::Text {
                    text: "calling".into(),
                },
                OutputItem::ToolCall {
                    id: "c1".into(),
                    name: "edit".into(),
                    input: serde_json::json!({}),
                },
            ],
            usage: Usage::default(),
            model: "test".into(),
        };
        assert!(resp.has_tool_calls());
    }

    #[test]
    fn test_model_provider_is_object() {
        let _boxed: Box<dyn ModelProvider> = Box::new(FakeProvider::text("x"));
    }

    #[test]
    fn test_request_response_serde_round() {
        let req = CompletionRequest {
            model: "m".into(),
            instructions: "sys".into(),
            input: vec![InputItem::User {
                content: "hi".into(),
            }],
            tools: vec![ToolDef {
                name: "edit".into(),
                description: "edit a file".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            settings: ModelSettings {
                temperature: Some(0.0),
                ..Default::default()
            },
            cache_breakpoints: Vec::new(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: CompletionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn test_provider_error_retryable() {
        assert!(ProviderError::Network.retryable());
        assert!(
            ProviderError::RateLimit {
                retry_after_ms: None
            }
            .retryable()
        );
        assert!(ProviderError::ProviderInternal.retryable());
        assert!(!ProviderError::Auth.retryable());
        assert!(!ProviderError::InvalidRequest("x".into()).retryable());
    }

    fn stub_req() -> CompletionRequest {
        CompletionRequest {
            model: "test".into(),
            instructions: String::new(),
            input: vec![InputItem::User {
                content: "hi".into(),
            }],
            tools: vec![],
            settings: ModelSettings::default(),
            cache_breakpoints: Vec::new(),
        }
    }

    #[test]
    fn test_fake_cycles_repeats_last() {
        // Two responses: first carries a tool call, second plain text. The
        // third call (past the list) must repeat the last, so a run that loops
        // past the sequence ends on the text response instead of re-emitting the
        // tool call forever.
        let p = FakeProvider::from_outputs(vec![
            vec![OutputItem::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "a"}),
            }],
            vec![OutputItem::Text {
                text: "done".into(),
            }],
        ]);
        let r1 = pollster::block_on(p.complete(stub_req())).expect("call 1");
        assert!(r1.has_tool_calls());
        let r2 = pollster::block_on(p.complete(stub_req())).expect("call 2");
        assert!(!r2.has_tool_calls());
        let r3 = pollster::block_on(p.complete(stub_req())).expect("call 3");
        assert!(
            !r3.has_tool_calls(),
            "call past the list must repeat the last (text) response"
        );
    }

    #[test]
    fn test_fake_stream_tool_call() {
        // The stream path delegates to stream_from_response, so a ToolCall
        // response surfaces a ToolCall event (the runner executes the tool).
        let p = FakeProvider::from_outputs(vec![vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "glob".into(),
            input: serde_json::json!({"pattern": "*.rs"}),
        }]]);
        let stream = p.stream(stub_req());
        use futures::StreamExt;
        let events: Vec<_> = pollster::block_on(async {
            let mut s = stream;
            let mut out = Vec::new();
            while let Some(ev) = s.next().await {
                out.push(ev);
            }
            out
        });
        assert!(
            events.iter().any(|e| matches!(
                e,
                Ok(houyicoder_protocol::llm::LlmEvent::ToolCall { name, .. }) if name == "glob"
            )),
            "stream must emit the ToolCall event: {events:?}"
        );
    }

    /// with_error returns the injected ProviderError on every complete() call.
    #[test]
    fn test_with_error_propagates() {
        let p = FakeProvider::with_error(ProviderError::Auth);
        let req = CompletionRequest {
            model: "test".into(),
            instructions: "".into(),
            input: vec![],
            tools: vec![],
            settings: ModelSettings::default(),
            cache_breakpoints: Vec::new(),
        };
        let err = pollster::block_on(p.complete(req)).unwrap_err();
        assert!(matches!(err, ProviderError::Auth), "injected Auth returned");

        let p2 = FakeProvider::with_error(ProviderError::ModelNotFound("x".into()));
        let err2 = pollster::block_on(p2.complete(CompletionRequest {
            model: "test".into(),
            instructions: "".into(),
            input: vec![],
            tools: vec![],
            settings: ModelSettings::default(),
            cache_breakpoints: Vec::new(),
        }))
        .unwrap_err();
        assert!(
            matches!(err2, ProviderError::ModelNotFound(_)),
            "injected ModelNotFound returned"
        );
    }

    /// ModelNotFound Display renders the model id in the message.
    #[test]
    fn test_model_not_found_display() {
        let e = ProviderError::ModelNotFound("qwen3.8-max".into());
        let s = e.to_string();
        assert!(s.contains("qwen3.8-max"), "id in display: {s}");
        assert!(s.contains("not found"), "not found in display: {s}");
    }
}
