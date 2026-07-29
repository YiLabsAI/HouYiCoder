//! Test-only fake provider for engine-internal tests. The ModelProvider
//! trait and the stream_from_response adapter live in the ports crate; this
//! module holds only the in-tree test fake. Production adapter impls live in
//! the provider crate, which has its own public FakeProvider for downstream
//! tests. The two copies exist because the engine crate does not depend on
//! the provider crate (layering); the behavior is identical.

#[cfg(test)]
pub(crate) mod test_support {
    use houyicoder_api::provider::{ModelProvider, stream_from_response};
    use houyicoder_async::{PFut, PStream};
    use houyicoder_protocol::llm::{
        CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem,
        ProviderError, Usage,
    };

    /// Deterministic fake provider (no LLM): returns a programmed sequence of
    /// canned responses, repeating the last forever. A single-element sequence
    /// is a stateless double that always returns that response. Following the
    /// test-double taxonomy (Meszaros), this is a fake — a working, simplified
    /// in-memory implementation — matching the LLM-tooling convention
    /// (LangChain FakeListChatModel).
    #[derive(Debug)]
    pub(crate) struct FakeProvider {
        responses: Vec<CompletionResponse>,
        call: std::sync::atomic::AtomicU32,
    }

    impl FakeProvider {
        /// Build from an ordered list of responses. The last is repeated for
        /// any model calls past the list length, so a run that loops past the
        /// sequence ends on the final response instead of re-emitting an
        /// earlier one. An empty list is allowed for placeholder providers
        /// that are never called; a call against an empty list panics (fail
        /// loudly on use, not on construction).
        pub(crate) fn new(responses: Vec<CompletionResponse>) -> Self {
            Self {
                responses,
                call: std::sync::atomic::AtomicU32::new(0),
            }
        }

        /// Convenience: a single text response (a one-element sequence that
        /// always returns that text).
        pub(crate) fn text(text: &str) -> Self {
            Self::new(vec![CompletionResponse {
                output: vec![OutputItem::Text {
                    text: text.to_string(),
                }],
                usage: Usage::default(),
                model: "test".to_string(),
            }])
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
            let resp = self.next_response();
            Box::pin(async move { Ok(resp) })
        }
        fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
            stream_from_response(self.next_response())
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }
}
