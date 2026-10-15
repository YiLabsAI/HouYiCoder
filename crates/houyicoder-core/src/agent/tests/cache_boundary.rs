//! The turn path puts the prompt-cache boundary on the request it sends, so
//! the provider hashes only the static segment of the instructions into the
//! cache key. A request that dropped the offset would rotate the key on every
//! working-directory change, so this drives a real turn and reads the offset
//! back off the captured request to confirm the turn path threads it.
use super::*;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::provider::stream_from_response;
use houyicoder_protocol::llm::ModelCapabilities;

/// A provider that records the dynamic boundary and the instructions length of
/// each request, then returns a final text so the run ends after one turn.
struct BoundaryCapture {
    seen: Mutex<Vec<(Option<usize>, usize)>>,
}

impl BoundaryCapture {
    fn new() -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
        }
    }
    fn record(&self, req: &CompletionRequest) {
        self.seen
            .lock()
            .expect("seen")
            .push((req.dynamic_boundary, req.instructions.len()));
    }
    fn final_response() -> CompletionResponse {
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }
    }
}

impl ModelProvider for BoundaryCapture {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> houyicoder_async::PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.record(&req);
        let resp = Self::final_response();
        Box::pin(async move { Ok(resp) })
    }
    fn stream(
        &self,
        req: CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
        self.record(&req);
        stream_from_response(Self::final_response())
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

#[tokio::test]
async fn test_turn_threads_boundary() {
    let capture = Arc::new(BoundaryCapture::new());
    let runner = runner_with(capture.clone(), ToolRegistry::new());
    runner
        .run(SessionId::new(), "hi".into())
        .await
        .expect("run completes");
    let seen = capture.seen.lock().expect("seen");
    let (boundary, len) = *seen.first().expect("the turn sent a request");
    let boundary = boundary.expect("the request carries a dynamic boundary");
    assert!(boundary > 0, "boundary is interior, not zero");
    assert!(boundary < len, "boundary precedes the dynamic suffix");
}
