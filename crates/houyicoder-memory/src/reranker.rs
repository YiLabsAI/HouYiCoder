//! The semantic selection stage of recall. Given a query and the ranked
//! candidate metadata, one one-shot model call picks the keys worth
//! injecting. The call is bounded by the host's deadline; every failure
//! mode returns a typed outcome so the host can fall back deterministically
//! and telemetry can name the reason.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use houyicoder_api::memory::{MemoryReranker, RerankOutcome};
use houyicoder_api::provider::ModelProvider;
use houyicoder_async::PFut;
use houyicoder_context::{MemoryRankHit, memory_age_days, memory_age_label};
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, InputItem, ModelSettings, OutputItem,
};

/// The selection instruction. The reply contract is a bare JSON array of
/// candidate keys so parsing has one shape to accept and everything else is
/// a typed malformed outcome.
const SELECTION_INSTRUCTION: &str = "You select which stored memories are relevant to a \
search query. Reply with only a JSON array of memory keys copied from the candidate \
list, most relevant first. Reply [] when no candidate is genuinely relevant to the \
query. Never invent keys and never reply with anything other than the JSON array.";

/// A model-backed key selector over ranked candidate metadata.
pub struct SemanticReranker {
    provider: Arc<dyn ModelProvider>,
    model: String,
}

impl SemanticReranker {
    /// Construct over the model the main loop uses.
    pub fn new(provider: Arc<dyn ModelProvider>, model: String) -> Self {
        Self { provider, model }
    }
}

impl MemoryReranker for SemanticReranker {
    fn rerank(
        &self,
        query: &str,
        candidates: &[MemoryRankHit],
        limit: usize,
    ) -> PFut<'_, RerankOutcome> {
        // No candidates is a nothing-to-select answer, not a model call:
        // spending a query to select from an empty list can only invent keys.
        if candidates.is_empty() || limit == 0 {
            return Box::pin(async { RerankOutcome::Selected(Vec::new()) });
        }
        let provider = Arc::clone(&self.provider);
        let model = self.model.clone();
        let instructions = format!("{SELECTION_INSTRUCTION} Select at most {limit} keys.");
        let input = build_input(query, candidates);
        // The candidate keys go in owned so the future borrows nothing.
        let keys: Vec<String> = candidates.iter().map(|c| c.key.clone()).collect();
        Box::pin(async move {
            let req = CompletionRequest {
                model,
                instructions,
                input: vec![InputItem::User { content: input }],
                tools: Vec::new(),
                settings: ModelSettings::default(),
                cache_breakpoints: Vec::new(),
            };
            match provider.complete(req).await {
                Ok(resp) => parse_selection(&resp, &keys, limit),
                Err(e) => RerankOutcome::Unavailable(e.to_string()),
            }
        })
    }
}

/// Render the query and the candidate metadata the model selects from. The
/// age is a human label because an epoch timestamp invites arithmetic the
/// model does badly.
fn build_input(query: &str, candidates: &[MemoryRankHit]) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut out = format!("Query: {query}\n\nCandidates:\n");
    for c in candidates {
        let age = memory_age_label(memory_age_days(c.mtime_secs, now));
        out.push_str(&format!(
            "- {} | source {} | scope {} | {} | {}\n",
            c.key,
            c.source.as_label(),
            c.scope.as_label(),
            age,
            c.description
        ));
    }
    out
}

/// Read the reply as a key selection. Keys outside the candidate list are
/// dropped, duplicates collapse, and the selection truncates at the limit.
/// A reply that is not a readable JSON array is a typed malformed outcome,
/// never a silent empty selection.
fn parse_selection(
    resp: &CompletionResponse,
    candidate_keys: &[String],
    limit: usize,
) -> RerankOutcome {
    let Some(text) = resp.output.iter().find_map(|item| match item {
        OutputItem::Text { text } => Some(text.as_str()),
        _ => None,
    }) else {
        return RerankOutcome::Malformed("reply carried no text".to_string());
    };
    let parsed: Vec<String> = match serde_json::from_str(strip_fences(text)) {
        Ok(keys) => keys,
        Err(e) => {
            return RerankOutcome::Malformed(format!("reply is not a JSON key array: {e}"));
        }
    };
    let mut selected: Vec<String> = Vec::new();
    for key in parsed {
        if selected.len() >= limit {
            break;
        }
        if !selected.contains(&key) && candidate_keys.contains(&key) {
            selected.push(key);
        }
    }
    RerankOutcome::Selected(selected)
}

/// Unwrap a reply the model fenced as a code block despite instructions.
fn strip_fences(text: &str) -> &str {
    let trimmed = text.trim();
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    body.strip_suffix("```").unwrap_or(body).trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_async::PStream;
    use houyicoder_context::{MemoryScope, MemorySource};
    use houyicoder_protocol::llm::{LlmEvent, ModelCapabilities, ProviderError, Usage};

    fn hit(key: &str) -> MemoryRankHit {
        MemoryRankHit::new(
            key,
            format!("{key} description"),
            MemorySource::Feedback,
            MemoryScope::Auto,
            0,
            0,
        )
    }

    /// A stub answering one canned completion.
    struct ReplyProvider(Vec<OutputItem>);

    impl ModelProvider for ReplyProvider {
        fn complete(
            &self,
            _req: CompletionRequest,
        ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
            let resp = CompletionResponse {
                output: self.0.clone(),
                usage: Usage::default(),
                model: "test".into(),
            };
            Box::pin(async move { Ok(resp) })
        }
        fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
            Box::pin(futures::stream::iter(Vec::new()))
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    fn text_reply(body: &str) -> Arc<dyn ModelProvider> {
        Arc::new(ReplyProvider(vec![OutputItem::Text {
            text: body.to_string(),
        }]))
    }

    fn candidates() -> Vec<MemoryRankHit> {
        vec![hit("deploy-gate"), hit("tea-order"), hit("cache-policy")]
    }

    async fn rerank(provider: Arc<dyn ModelProvider>, limit: usize) -> RerankOutcome {
        let r = SemanticReranker::new(provider, "test".into());
        r.rerank("deploy question", &candidates(), limit).await
    }

    #[tokio::test]
    async fn test_rerank_selects_listed_keys() {
        let out = rerank(text_reply(r#"["tea-order", "deploy-gate"]"#), 5).await;
        assert_eq!(
            out,
            RerankOutcome::Selected(vec!["tea-order".into(), "deploy-gate".into()]),
            "the model's relevance order is preserved"
        );
    }

    #[tokio::test]
    async fn test_rerank_drops_invented_keys() {
        let out = rerank(text_reply(r#"["ghost", "tea-order", "tea-order"]"#), 5).await;
        assert_eq!(
            out,
            RerankOutcome::Selected(vec!["tea-order".into()]),
            "a key outside the candidate list never passes, duplicates collapse"
        );
    }

    #[tokio::test]
    async fn test_rerank_truncates_at_limit() {
        let out = rerank(
            text_reply(r#"["deploy-gate", "tea-order", "cache-policy"]"#),
            2,
        )
        .await;
        assert_eq!(
            out,
            RerankOutcome::Selected(vec!["deploy-gate".into(), "tea-order".into()])
        );
    }

    /// An empty array is the model's confident nothing-is-relevant verdict —
    /// a Selected outcome, not a failure the host falls back from.
    #[tokio::test]
    async fn test_rerank_empty_is_selected() {
        let out = rerank(text_reply("[]"), 5).await;
        assert_eq!(out, RerankOutcome::Selected(Vec::new()));
    }

    #[tokio::test]
    async fn test_rerank_accepts_fenced_json() {
        let out = rerank(text_reply("```json\n[\"tea-order\"]\n```"), 5).await;
        assert_eq!(out, RerankOutcome::Selected(vec!["tea-order".into()]));
    }

    #[tokio::test]
    async fn test_rerank_prose_is_malformed() {
        let out = rerank(text_reply("I would pick tea-order."), 5).await;
        assert!(
            matches!(out, RerankOutcome::Malformed(_)),
            "prose is a typed malformed outcome, never a silent empty: {out:?}"
        );
    }

    #[tokio::test]
    async fn test_rerank_textless_malformed() {
        let provider: Arc<dyn ModelProvider> = Arc::new(ReplyProvider(Vec::new()));
        let out = rerank(provider, 5).await;
        assert!(matches!(out, RerankOutcome::Malformed(_)), "{out:?}");
    }

    #[tokio::test]
    async fn test_rerank_error_is_unavailable() {
        struct Failing;
        impl ModelProvider for Failing {
            fn complete(
                &self,
                _req: CompletionRequest,
            ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
                Box::pin(async { Err(ProviderError::Unknown("no route".into())) })
            }
            fn stream(
                &self,
                _req: CompletionRequest,
            ) -> PStream<'_, Result<LlmEvent, ProviderError>> {
                Box::pin(futures::stream::iter(Vec::new()))
            }
            fn capabilities(&self) -> ModelCapabilities {
                ModelCapabilities::default()
            }
        }
        let provider: Arc<dyn ModelProvider> = Arc::new(Failing);
        let out = rerank(provider, 5).await;
        assert!(
            matches!(&out, RerankOutcome::Unavailable(reason) if reason.contains("no route")),
            "the provider reason survives in the outcome: {out:?}"
        );
    }

    /// An empty candidate list answers without spending a model call, and
    /// the prompt names every metadata column the selection reads.
    #[tokio::test]
    async fn test_rerank_skips_empty_list() {
        struct Counting;
        impl ModelProvider for Counting {
            fn complete(
                &self,
                _req: CompletionRequest,
            ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
                panic!("an empty candidate list must not reach the model")
            }
            fn stream(
                &self,
                _req: CompletionRequest,
            ) -> PStream<'_, Result<LlmEvent, ProviderError>> {
                Box::pin(futures::stream::iter(Vec::new()))
            }
            fn capabilities(&self) -> ModelCapabilities {
                ModelCapabilities::default()
            }
        }
        let r = SemanticReranker::new(Arc::new(Counting), "test".into());
        let out = r.rerank("anything", &[], 5).await;
        assert_eq!(out, RerankOutcome::Selected(Vec::new()));
    }

    #[test]
    fn test_input_names_every_column() {
        let input = build_input("deploy question", &candidates());
        assert!(input.contains("Query: deploy question"));
        assert!(input.contains("deploy-gate"), "{input}");
        assert!(input.contains("source feedback"), "{input}");
        assert!(input.contains("scope auto"), "{input}");
        assert!(input.contains("deploy-gate description"), "{input}");
    }
}
