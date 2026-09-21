//! The streaming usage object arrives in a trailing chunk whose choices
//! array is empty, after the chunk that carries finish_reason. A reader that
//! stops at finish_reason loses the token counts, so this pins that the final
//! usage (output tokens, cache reads, reasoning) survives the stream.

use futures::StreamExt;
use houyicoder_api::provider::ModelProvider;
use houyicoder_protocol::llm::{CompletionRequest, InputItem, LlmEvent, ModelSettings};
use houyicoder_provider::OpenAiCompatibleProvider;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A provider stream that closes the choice stream first and sends usage in a
/// later chunk, matching the OpenAI and DashScope wire order.
fn sse_body() -> String {
    let chunks = [
        r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}],"usage":null}"#,
        r#"{"choices":[{"index":0,"delta":{"content":""},"finish_reason":"stop"}],"usage":null}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":16095,"completion_tokens":12,"total_tokens":16107,"prompt_tokens_details":{"cached_tokens":12288},"completion_tokens_details":{"reasoning_tokens":10}}}"#,
        "[DONE]",
    ];
    chunks
        .iter()
        .map(|c| format!("data: {c}\n\n"))
        .collect::<String>()
}

fn request() -> CompletionRequest {
    CompletionRequest {
        model: "test-model".into(),
        instructions: "you are a test agent".into(),
        input: vec![InputItem::User {
            content: "hello".into(),
        }],
        tools: vec![],
        settings: ModelSettings::default(),
        cache_breakpoints: Vec::new(),
    }
}

#[tokio::test]
async fn test_usage_after_finish_reason() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body()),
        )
        .mount(&server)
        .await;

    let provider = OpenAiCompatibleProvider::new(server.uri(), "test-key".into());
    let mut stream = provider.stream(request());
    let mut finish_usage = None;
    while let Some(ev) = stream.next().await {
        if let Ok(LlmEvent::Finish { reason, usage }) = ev {
            assert_eq!(reason, "stop", "finish reason preserved");
            finish_usage = Some(usage);
        }
    }
    let usage = finish_usage
        .expect("stream emits a Finish event")
        .expect("finish carries usage");
    assert_eq!(
        usage.output_tokens, 12,
        "completion tokens from the trailing usage chunk"
    );
    assert_eq!(
        usage.cache_read_input_tokens, 12288,
        "cache reads from the trailing usage chunk"
    );
    assert_eq!(usage.reasoning_tokens, 10, "reasoning tokens preserved");
    assert_eq!(usage.input_tokens, 16095, "prompt tokens preserved");
}
