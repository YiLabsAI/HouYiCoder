//! Live integration test: real call against an OpenAI-compatible endpoint
//! via DASHSCOPE credentials. Skips silently when no API key is set, so a
//! bare make test-integration never fails on a machine without credentials.
//! Run with: make test-integration (after writing .env or exporting the vars).

use houyicoder_api::provider::ModelProvider;
use houyicoder_config::{DEFAULT_DASHSCOPE_BASE_URL, DEFAULT_MODEL};
use houyicoder_protocol::llm::{CompletionRequest, InputItem, ModelSettings, OutputItem};
use houyicoder_provider::OpenAiCompatibleProvider;

#[tokio::test]
async fn test_openai_roundtrip() {
    let Ok(api_key) = std::env::var("DASHSCOPE_API_KEY") else {
        eprintln!("skip: DASHSCOPE_API_KEY not set");
        return;
    };
    let base_url =
        std::env::var("DASHSCOPE_BASE_URL").unwrap_or_else(|_| DEFAULT_DASHSCOPE_BASE_URL.into());
    let model = std::env::var("HOUYICODER_TEST_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
    let provider = OpenAiCompatibleProvider::new(base_url, api_key);
    let req = CompletionRequest {
        model,
        instructions: "Reply with exactly: pong".into(),
        input: vec![InputItem::User {
            content: "ping".into(),
        }],
        tools: vec![],
        settings: ModelSettings {
            max_output_tokens: Some(64),
            ..Default::default()
        },
        cache_breakpoints: Vec::new(),
    };
    let resp = provider.complete(req).await.expect("real call succeeds");
    assert!(!resp.output.is_empty(), "response had no output items");
    let has_text = resp
        .output
        .iter()
        .any(|o| matches!(o, OutputItem::Text { .. }));
    assert!(
        has_text,
        "expected at least one text output, got {:?}",
        resp.output
    );
}
