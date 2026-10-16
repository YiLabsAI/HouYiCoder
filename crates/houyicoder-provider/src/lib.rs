//! Provider adapters and the resolution that builds them: FakeProvider and
//! OpenAiCompatibleProvider. The ModelProvider trait and the LLM data types
//! live elsewhere (ports::provider and protocol::llm); this crate holds the
//! adapter impls that satisfy the port, plus the step that turns a project's
//! settings into one of them.

use houyicoder_api::provider::ModelProvider;
use houyicoder_async::PFut;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};

mod api_key;
mod http_error;
mod openai_compat;
mod resolution;
mod served_models;
mod stream_decoder;
mod usage;

pub use resolution::{ProviderFacts, ProviderResolution, resolve};

use futures::StreamExt;
/// Adapt a complete (non-streaming) response into a stream of LlmEvents, so a
/// provider that only knows how to produce a CompletionResponse can still satisfy
/// the stream() seam — and so tests with scripted responses plug into the
/// streaming loop without re-implementing the event taxonomy. Text is chunked
/// into 4-char deltas to mimic token-by-token delivery; tool calls and reasoning
/// are emitted as single events. Re-exported from ports so adapter crates and
/// consumers share one copy.
pub use houyicoder_api::provider::stream_from_response;
pub use openai_compat::OpenAiCompatibleProvider;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::sleep;

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
    call: AtomicU32,
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
            call: AtomicU32::new(0),
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
            call: AtomicU32::new(0),
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
            call: AtomicU32::new(0),
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
                sleep(Duration::from_millis(ms)).await;
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
mod tests;
