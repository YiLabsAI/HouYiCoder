//! The streaming chat-completion port the engine consumes. The concrete
//! client lives in the provider crate; the engine depends on this trait,
//! not the concrete implementation. Relocated here from the engine so the
//! provider depends downward, not back into the engine.

use houyicoder_async::{PFut, PStream};
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
};

/// The provider-agnostic LLM seam. Object-safe (PFut) so the loop holds
/// Box<dyn ModelProvider> and swaps an OpenAI-compatible, local, or stub
/// impl behind it.
pub trait ModelProvider: Send + Sync {
    /// Run one non-streaming completion. The loop wraps this in
    /// Retry (with replay-safety vetoes for stateful conversations).
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>>;

    /// Stream a completion as a sequence of LlmEvent (text deltas, tool
    /// calls, usage, finish). The agent loop consumes this for live
    /// token-by-token TUI output. Providers implement this; complete() is
    /// a default that collects the stream.
    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>>;

    /// What this model can do (capability negotiation).
    fn capabilities(&self) -> ModelCapabilities;

    /// Ask the provider which model ids it actually serves, and cache the
    /// answer for catalog existence validation. Fire-and-forget at startup:
    /// the host spawns this on the runtime and never awaits it. Default
    /// no-op (stubs + providers without a /v1/models endpoint do nothing —
    /// the existence check stays skipped, never errors). A real impl writes
    /// the served-models cache; failure degrades (keeps the old cache).
    fn refresh_served_models(&self) -> PFut<'_, Result<(), ProviderError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Adapt a complete (non-streaming) response into a stream of LlmEvents, so a
/// provider that only knows how to produce a CompletionResponse can still satisfy
/// the stream() seam — and so tests with scripted responses plug into the
/// streaming loop without re-implementing the event taxonomy. Text is chunked
/// into 4-char deltas to mimic token-by-token delivery; tool calls and reasoning
/// are emitted as single events. This is also the default complete()-from-stream
/// path's inverse, used by FakeProvider and test script providers.
pub fn stream_from_response(
    resp: CompletionResponse,
) -> PStream<'static, Result<LlmEvent, ProviderError>> {
    let usage = resp.usage;
    // Test affordance: an inter-chunk delay so the stub run stays in-flight
    // long enough for PTY UI tests to drive mid-run keys (e.g. a Shift+Tab
    // mode cycle while agent_busy). Only active when HOUYICODER_STUB_DELAY_MS
    // is set; zero/absent = the default back-to-back stream. The stub exists
    // for dev/test, so a delay knob is an honest test affordance, not a
    // feature. Prefixed HOUYICODER_ to match the other env knobs.
    let delay_ms = std::env::var("HOUYICODER_STUB_DELAY_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|ms| *ms > 0);
    let stream = async_stream::stream! {
        yield Ok(LlmEvent::StepStart { index: 0 });
        let mut n = 0u32;
        for item in &resp.output {
            match item {
                OutputItem::Text { text } => {
                    let id = format!("text-{n}");
                    n += 1;
                    yield Ok(LlmEvent::TextStart { id: id.clone() });
                    let mut chars = text.chars().peekable();
                    while chars.peek().is_some() {
                        let chunk: String = chars.by_ref().take(4).collect();
                        if !chunk.is_empty() {
                            // Delay BEFORE each delta (including the first) so the
                            // stub stream starts after delay_ms, not immediately.
                            // A pre-content in-flight window is what the PTY
                            // abort/abort-restore tests need: the first delta
                            // must not land before an Esc sent right after Enter
                            // cancels the run, otherwise run_produced_real_content
                            // is true and the restore path never fires.
                            if let Some(ms) = delay_ms {
                                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                            }
                            yield Ok(LlmEvent::TextDelta { id: id.clone(), text: chunk });
                        }
                    }
                    yield Ok(LlmEvent::TextEnd { id });
                }
                OutputItem::Reasoning { text } => {
                    let id = format!("reason-{n}");
                    n += 1;
                    yield Ok(LlmEvent::ReasoningStart { id: id.clone() });
                    yield Ok(LlmEvent::ReasoningDelta { id: id.clone(), text: text.clone() });
                    yield Ok(LlmEvent::ReasoningEnd { id });
                }
                OutputItem::ToolCall { id, name, input } => {
                    yield Ok(LlmEvent::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    });
                }
            }
        }
        yield Ok(LlmEvent::StepFinish {
            index: 0,
            reason: "stop".into(),
            usage: None,
        });
        yield Ok(LlmEvent::Finish {
            reason: "stop".into(),
            usage: Some(usage),
        });
    };
    Box::pin(stream)
}
