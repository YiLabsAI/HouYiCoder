//! Tests for this module.

use std::sync::Mutex;

use super::*;
use futures::stream::iter;
use houyicoder_async::PStream;
use houyicoder_context::{MemoryScope, MemoryType};
use houyicoder_protocol::llm::{LlmEvent, ModelCapabilities, ProviderError, Usage};

fn hit(key: &str) -> MemoryRankHit {
    MemoryRankHit::new(
        key,
        format!("{key} description"),
        MemoryType::Feedback,
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
        Box::pin(iter(Vec::new()))
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
        fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
            Box::pin(iter(Vec::new()))
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
        fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
            Box::pin(iter(Vec::new()))
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
    assert!(input.contains("type feedback"), "{input}");
    assert!(input.contains("scope auto"), "{input}");
    assert!(input.contains("deploy-gate description"), "{input}");
}

/// The reranker configures low thinking budget and low reasoning effort so
/// a thinking model does not burn its token budget before emitting the JSON array.
#[tokio::test]
async fn test_reranker_disables_thinking() {
    struct Capturing(Mutex<Option<CompletionRequest>>);
    impl ModelProvider for Capturing {
        fn complete(
            &self,
            req: CompletionRequest,
        ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
            *self.0.lock().expect("lock") = Some(req);
            let resp = CompletionResponse {
                output: vec![OutputItem::Text {
                    text: r#"["tea-order"]"#.into(),
                }],
                usage: Usage::default(),
                model: "test".into(),
            };
            Box::pin(async move { Ok(resp) })
        }
        fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
            Box::pin(iter(Vec::new()))
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }
    let cap = Arc::new(Capturing(Mutex::new(None)));
    let r = SemanticReranker::new(cap.clone(), "test".into());
    let _outcome = r.rerank("query", &candidates(), 5).await;
    let req = cap
        .0
        .lock()
        .expect("lock")
        .take()
        .expect("request captured");
    assert_eq!(req.settings.max_output_tokens, Some(160));
    assert_eq!(req.settings.enable_thinking, Some(false));
    assert_eq!(req.settings.reasoning_effort, Some(EffortLevel::Low));
}
