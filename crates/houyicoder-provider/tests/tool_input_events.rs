//! A tool call arrives as a stream of argument fragments, and the stream has
//! to publish them as they come: a caller times the model from the first
//! fragment and reads what the arguments cost, neither of which it can do from
//! the call alone, which lands once the fragment stream has ended.

use futures::StreamExt;
use houyicoder_api::provider::ModelProvider;
use houyicoder_protocol::cache_policy::PromptCacheFormat;
use houyicoder_protocol::llm::{CompletionRequest, InputItem, LlmEvent, ModelSettings};
use houyicoder_provider::OpenAiCompatibleProvider;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The SSE body of a reply whose single call is sent in two fragments.
fn sse_body(chunks: &[&str]) -> String {
    chunks
        .iter()
        .map(|c| format!("data: {c}\n\n"))
        .collect::<String>()
}

fn request() -> CompletionRequest {
    CompletionRequest {
        model: "test-model".into(),
        instructions: "you are a test agent".into(),
        dynamic_boundary: None,
        input: vec![InputItem::User {
            content: "hello".into(),
        }],
        tools: vec![],
        settings: ModelSettings::default(),
        cache_breakpoints: Vec::new(),
        cache_format: PromptCacheFormat::AutoPrefix,
    }
}

/// Drive one reply through a mock endpoint and return the events it yielded.
async fn events_of(body: String) -> Vec<LlmEvent> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let provider = OpenAiCompatibleProvider::new(server.uri(), "test-key".into());
    let mut stream = provider.stream(request());
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev.expect("the stream carries no error"));
    }
    events
}

/// The streamed reply publishes the arguments as they arrive and then the call
/// they built: the reader sees the block open, grow, and close, and the
/// fragments it read spell out the call's input.
#[tokio::test]
async fn test_tool_call_streams_arguments() {
    let events = events_of(sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"tc1","type":"function","function":{"name":"read_file","arguments":"{\"path\":"}}]},"finish_reason":null}],"usage":null}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]},"finish_reason":null}],"usage":null}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":null}"#,
        "[DONE]",
    ]))
    .await;

    let shape: Vec<&str> = events
        .iter()
        .map(|e| match e {
            LlmEvent::ToolInputStart { .. } => "start",
            LlmEvent::ToolInputDelta { .. } => "delta",
            LlmEvent::ToolInputEnd { .. } => "end",
            LlmEvent::ToolCall { .. } => "call",
            _ => "other",
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            "other", "start", "delta", "delta", "end", "call", "other", "other"
        ],
        "the arguments open, grow, close, then the call they built: {events:?}"
    );

    let printed: Vec<(String, String)> = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::ToolInputDelta { name, text, .. } => Some((name.clone(), text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        printed,
        vec![
            ("read_file".to_string(), "{\"path\":".to_string()),
            ("read_file".to_string(), "\"a.rs\"}".to_string()),
        ],
        "each fragment is published with the call it belongs to"
    );

    let call = events
        .iter()
        .find_map(|e| match e {
            LlmEvent::ToolCall { input, .. } => Some(input.clone()),
            _ => None,
        })
        .expect("the stream ends with the call");
    let read: serde_json::Value = serde_json::from_str(
        &events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::ToolInputDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>(),
    )
    .expect("the published fragments parse as the arguments");
    assert_eq!(
        read, call,
        "the fragments the reader saw spell out the call's input"
    );
}

/// A call whose arguments arrive whole in the fragment that names it still
/// opens and closes one block: a reader waiting for a delta is not left
/// waiting on a call that had none.
#[tokio::test]
async fn test_whole_arguments_open_once() {
    let events = events_of(sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"tc1","type":"function","function":{"name":"echo","arguments":"{}"}}]},"finish_reason":null}],"usage":null}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":null}"#,
        "[DONE]",
    ]))
    .await;

    let starts = events
        .iter()
        .filter(|e| matches!(e, LlmEvent::ToolInputStart { .. }))
        .count();
    let ends = events
        .iter()
        .filter(|e| matches!(e, LlmEvent::ToolInputEnd { .. }))
        .count();
    assert_eq!(
        (starts, ends),
        (1, 1),
        "one call opens and closes one argument block: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            LlmEvent::ToolCall { input, .. } if *input == serde_json::json!({})
        )),
        "the call carries the whole arguments: {events:?}"
    );
}
