use super::*;
use futures::StreamExt;
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
            Ok(LlmEvent::ToolCall { name, .. }) if name == "glob"
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
