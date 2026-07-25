use super::*;
use crate::http_error::{classify_status, classify_with_body, parse_retry_after};
use houyicoder_protocol::llm::{AssistantToolCall, EffortLevel, ModelSettings, ToolDef};

fn req(input: Vec<InputItem>, tools: Vec<ToolDef>) -> CompletionRequest {
    CompletionRequest {
        model: "test-model".into(),
        instructions: "you are a test".into(),
        input,
        tools,
        settings: ModelSettings {
            max_output_tokens: Some(8000),
            temperature: Some(0.0),
            ..Default::default()
        },
        cache_breakpoints: Vec::new(),
    }
}

#[test]
fn test_build_body_user_assistant() {
    let r = req(
        vec![
            InputItem::User {
                content: "hi".into(),
            },
            InputItem::Assistant {
                content: "let me call".into(),
                tool_calls: vec![AssistantToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    input: json!({"x": 1}),
                }],
            },
            InputItem::ToolResult {
                call_id: "c1".into(),
                output: json!({"echo": {"x": 1}}),
            },
        ],
        vec![],
    );
    let body = build_request_body(&r);
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["stream"], false);
    assert_eq!(body["max_tokens"], 8000);
    assert_eq!(body["temperature"], 0.0);
    let msgs = body["messages"].as_array().unwrap();
    // system + user + assistant + tool = 4.
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[1]["role"], "user");
    assert_eq!(msgs[2]["role"], "assistant");
    assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "echo");
    // arguments is a JSON string per OpenAI spec.
    assert!(msgs[2]["tool_calls"][0]["function"]["arguments"].is_string());
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], "c1");
}

#[test]
fn test_build_body_includes_tools() {
    let r = req(
        vec![InputItem::User {
            content: "hi".into(),
        }],
        vec![ToolDef {
            name: "edit".into(),
            description: "edit a file".into(),
            input_schema: json!({"type": "object"}),
        }],
    );
    let body = build_request_body(&r);
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "edit");
}

/// The SystemStaticPrefix cache breakpoint lowers to an OpenAI prompt_cache_key
/// so the provider's auto-cache reuses the stable system+tools prefix across
/// requests. Without a breakpoint (the None policy), no key is sent.
#[test]
fn test_prompt_cache_key_lowered() {
    use houyicoder_protocol::cache_policy::{BreakpointKind, CacheBreakpoint, CacheHint, CacheTtl};

    let mut r = req(
        vec![InputItem::User {
            content: "hi".into(),
        }],
        vec![ToolDef {
            name: "edit".into(),
            description: "edit a file".into(),
            input_schema: json!({"type": "object"}),
        }],
    );
    // No breakpoints (None policy): no prompt_cache_key on the wire.
    let body = build_request_body(&r);
    assert!(
        body.get("prompt_cache_key").is_none(),
        "no breakpoint ⇒ no cache key"
    );

    // Auto policy places the SystemStaticPrefix breakpoint.
    r.cache_breakpoints = vec![CacheBreakpoint {
        kind: BreakpointKind::SystemStaticPrefix,
        hint: CacheHint::Ephemeral(CacheTtl::OneHour),
    }];
    let body = build_request_body(&r);
    let key = body["prompt_cache_key"].as_str().expect("key lowered");
    assert!(
        key.starts_with("houyi-"),
        "key carries the label prefix, got: {key}"
    );

    // The same prefix hashes to the same key (stable across turns).
    let body2 = build_request_body(&r);
    assert_eq!(
        body2["prompt_cache_key"].as_str(),
        Some(key),
        "same prefix ⇒ same cache key"
    );

    // A changed instruction (different prefix) produces a different key.
    r.instructions = "you are a different agent".into();
    let body3 = build_request_body(&r);
    assert_ne!(
        body3["prompt_cache_key"].as_str(),
        Some(key),
        "changed prefix ⇒ different cache key"
    );
}

/// The cache key is derived from the stable prefix (instructions + tools)
/// only, never from the input history. Two requests that share a system
/// prompt + tool set but carry different conversation history — including
/// a prior turn's reasoning, which is history-only and must not enter the
/// cache-relevant hash — reuse the same key. A key that grew with history
/// would never hit; a key that folded in history-only reasoning would
/// thrash the prefix cache on every reasoning-bearing turn.
#[test]
fn test_cache_key_excludes_history() {
    use houyicoder_protocol::cache_policy::{BreakpointKind, CacheBreakpoint, CacheHint, CacheTtl};

    let tools = vec![ToolDef {
        name: "edit".into(),
        description: "edit a file".into(),
        input_schema: json!({"type": "object"}),
    }];
    let bp = vec![CacheBreakpoint {
        kind: BreakpointKind::SystemStaticPrefix,
        hint: CacheHint::Ephemeral(CacheTtl::OneHour),
    }];
    let mut r = req(
        vec![InputItem::User {
            content: "hi".into(),
        }],
        tools.clone(),
    );
    r.cache_breakpoints = bp.clone();
    let key = build_request_body(&r)["prompt_cache_key"]
        .as_str()
        .expect("key lowered")
        .to_string();

    // Same instructions + tools, but a longer history (extra user message,
    // an assistant turn with a tool call, a tool result). The key is stable.
    r.input = vec![
        InputItem::User {
            content: "first turn".into(),
        },
        InputItem::Assistant {
            content: "calling edit".into(),
            tool_calls: vec![AssistantToolCall {
                id: "c1".into(),
                name: "edit".into(),
                input: json!({"path": "a.rs"}),
            }],
        },
        InputItem::ToolResult {
            call_id: "c1".into(),
            output: json!({"ok": true}),
        },
        InputItem::User {
            content: "second turn".into(),
        },
    ];
    let body = build_request_body(&r);
    let key_with_history = body["prompt_cache_key"]
        .as_str()
        .expect("key still lowered");
    assert_eq!(
        key_with_history, key,
        "input history must not enter the cache-relevant hash"
    );
}

#[test]
fn test_parse_text_and_calls() {
    let json = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "calling echo",
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": {"name": "echo", "arguments": "{\"y\": 2}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    });
    let resp = parse_response(&json, "test-model").unwrap();
    assert_eq!(resp.model, "test-model");
    assert_eq!(resp.output.len(), 2);
    assert!(matches!(resp.output[0], OutputItem::Text { ref text } if text == "calling echo"));
    match &resp.output[1] {
        OutputItem::ToolCall { id, name, input } => {
            assert_eq!(id, "c1");
            assert_eq!(name, "echo");
            assert_eq!(input, &json!({"y": 2}));
        }
        _ => panic!("expected tool call"),
    }
    assert_eq!(resp.usage.input_tokens, 10);
    assert_eq!(resp.usage.output_tokens, 5);
    assert_eq!(resp.usage.total_tokens, 15);
}

#[test]
fn test_parse_response_malformed_args() {
    let json = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": {"name": "echo", "arguments": "not json{"}
                }]
            }
        }],
    });
    let resp = parse_response(&json, "m").unwrap();
    match &resp.output[0] {
        OutputItem::ToolCall { input, .. } => assert_eq!(input, &json!({})),
        _ => panic!("expected tool call with fallback object"),
    }
}

#[test]
fn test_parse_response_no_choices() {
    let json = json!({"choices": []});
    assert!(parse_response(&json, "m").is_err());
}

#[test]
fn test_classify_status_retryable_split() {
    // 429 → RateLimit (retryable); 5xx → ProviderInternal (retryable);
    // 408 → Network (retryable). 401 → Auth (not). 413 → ContextOverflow
    // (not). 422 → InvalidRequest (not).
    assert!(classify_status(429, None).retryable());
    assert!(classify_status(503, None).retryable());
    assert!(classify_status(408, None).retryable());
    assert!(!classify_status(401, None).retryable());
    assert!(!classify_status(413, None).retryable());
    assert!(!classify_status(422, None).retryable());
    assert!(matches!(
        classify_status(429, Some(5000)),
        ProviderError::RateLimit {
            retry_after_ms: Some(5000)
        }
    ));
    assert!(matches!(
        classify_status(413, None),
        ProviderError::ContextOverflow { .. }
    ));
}

#[test]
fn test_parse_retry_after_integer() {
    let mut h = reqwest::header::HeaderMap::new();
    assert!(parse_retry_after(&h).is_none());
    h.insert("retry-after", "30".parse().unwrap());
    assert_eq!(parse_retry_after(&h), Some(30_000));
}

#[test]
fn test_classify_body_quota_vs() {
    // 429 with a quota-exhaustion body upgrades to QuotaExceeded (not retryable,
    // so the loop won't burn budget retrying a billing wall).
    assert!(matches!(
        classify_with_body(429, None, "your quota has been exceeded"),
        ProviderError::QuotaExceeded
    ));
    assert!(matches!(
        classify_with_body(429, None, "insufficient_quota for this model"),
        ProviderError::QuotaExceeded
    ));
    assert!(!classify_with_body(429, None, "quota exceeded").retryable());
    // 429 with a plain rate-limit body stays RateLimit (retryable).
    assert!(matches!(
        classify_with_body(429, Some(5000), "rate limited, slow down"),
        ProviderError::RateLimit {
            retry_after_ms: Some(5000)
        }
    ));
    assert!(classify_with_body(429, None, "rate limited").retryable());
    // Empty body falls back to classify_status (RateLimit).
    assert!(matches!(
        classify_with_body(429, None, ""),
        ProviderError::RateLimit { .. }
    ));
}

#[test]
fn test_classify_body_context_overflow() {
    // 400/422 with a context-length body upgrades to ContextOverflow (the
    // compaction trigger signal), not a generic InvalidRequest.
    assert!(matches!(
        classify_with_body(400, None, "context_length_exceeded"),
        ProviderError::ContextOverflow { .. }
    ));
    assert!(matches!(
        classify_with_body(422, None, "maximum context length is 8192"),
        ProviderError::ContextOverflow {
            enforced_limit: Some(8192)
        }
    ));
    assert!(!classify_with_body(400, None, "context overflow").retryable());
    // 400 with an unrelated body stays InvalidRequest.
    assert!(matches!(
        classify_with_body(400, None, "invalid model id"),
        ProviderError::ModelNotFound(_)
    ));
    // 413 upgrades by status even without a context body (classify_status fallback).
    assert!(matches!(
        classify_with_body(413, None, "payload too large"),
        ProviderError::ContextOverflow {
            enforced_limit: None
        }
    ));
}

#[test]
fn test_accumulate_and_finalize_tool() {
    // OpenAI streams a tool call across chunks: first carries id + name,
    // following chunks concatenate arguments fragments. Reassembly must
    // yield one ToolCall with the parsed arguments.
    let mut acc: Vec<ToolCallAccum> = Vec::new();
    accumulate_tool_call(
        &mut acc,
        &json!({"index": 0, "id": "call_1", "function": {"name": "bash", "arguments": "{\"comm"}}),
    );
    accumulate_tool_call(
        &mut acc,
        &json!({"index": 0, "function": {"arguments": "and\":\"ls\"}"}}),
    );
    let events = finalize_tool_calls(acc);
    assert_eq!(events.len(), 1);
    match &events[0] {
        LlmEvent::ToolCall { id, name, input } => {
            assert_eq!(id, "call_1");
            assert_eq!(name, "bash");
            assert_eq!(input["command"], "ls");
        }
        _ => panic!("expected ToolCall"),
    }
}

#[test]
fn test_finalize_orders_calls() {
    // Two tool calls interleaved by index reassemble to two events, in
    // index order, each with its own id/name/args.
    let mut acc: Vec<ToolCallAccum> = Vec::new();
    accumulate_tool_call(
        &mut acc,
        &json!({"index": 1, "id": "b", "function": {"name": "edit", "arguments": "{\"p\":"}}),
    );
    accumulate_tool_call(
        &mut acc,
        &json!({"index": 0, "id": "a", "function": {"name": "bash", "arguments": "{\"c\":"}}),
    );
    accumulate_tool_call(
        &mut acc,
        &json!({"index": 0, "function": {"arguments": "\"ls\"}"}}),
    );
    accumulate_tool_call(
        &mut acc,
        &json!({"index": 1, "function": {"arguments": "1}"}}),
    );
    let events = finalize_tool_calls(acc);
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], LlmEvent::ToolCall { name, .. } if name == "bash"));
    assert!(matches!(&events[1], LlmEvent::ToolCall { name, .. } if name == "edit"));
}

#[test]
fn test_finalize_malformed_args_fallback() {
    // Incomplete arguments (stream cut mid-fragment) must not panic; they
    // fall back to an empty object so the loop gets a ToolCall, not a crash.
    let mut acc: Vec<ToolCallAccum> = Vec::new();
    accumulate_tool_call(
        &mut acc,
        &json!({"index": 0, "id": "x", "function": {"name": "bash", "arguments": "{\"comm"}}),
    );
    let events = finalize_tool_calls(acc);
    assert_eq!(events.len(), 1);
    match &events[0] {
        LlmEvent::ToolCall { name, input, .. } => {
            assert_eq!(name, "bash");
            assert!(input.is_object());
            assert!(input.as_object().unwrap().is_empty());
        }
        _ => panic!("expected ToolCall"),
    }
}

#[test]
fn test_parse_usage_reasoning() {
    // A thinking-model usage on an OpenAI-compat provider nests the reasoning
    // count under completion_tokens_details. Without reading it the reasoning
    // budget reads 0 and the visible-output accessor (= output - reasoning)
    // lies. Both are subsets of their parent totals.
    let u = json!({
        "prompt_tokens": 100,
        "completion_tokens": 80,
        "total_tokens": 180,
        "completion_tokens_details": {"reasoning_tokens": 30}
    });
    let usage = parse_usage(Some(&u));
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 80);
    assert_eq!(usage.reasoning_tokens, 30);
    assert_eq!(usage.visible_output_tokens(), 50, "80 - 30 reasoning");
}

#[test]
fn test_parse_usage_cached() {
    // The cached prefix nests under prompt_tokens_details.cached_tokens; it is
    // a subset of prompt_tokens. non_cached is the remainder, cache_read the
    // cached slice. Without this the cache-read column stays 0 even when the
    // provider reports a cached prefix.
    let u = json!({
        "prompt_tokens": 1000,
        "completion_tokens": 40,
        "total_tokens": 1040,
        "prompt_tokens_details": {"cached_tokens": 700}
    });
    let usage = parse_usage(Some(&u));
    assert_eq!(usage.input_tokens, 1000);
    assert_eq!(usage.cache_read_input_tokens, 700);
    assert_eq!(usage.non_cached_input_tokens, 300, "1000 - 700 cached");
    assert_eq!(usage.reasoning_tokens, 0, "no reasoning reported");
}

#[test]
fn test_parse_usage_defaults() {
    // A non-thinking provider omits the details objects entirely; the
    // breakdown fields default to 0 and non_cached equals the full prompt.
    let u = json!({"prompt_tokens": 50, "completion_tokens": 12, "total_tokens": 62});
    let usage = parse_usage(Some(&u));
    assert_eq!(usage.reasoning_tokens, 0);
    assert_eq!(usage.cache_read_input_tokens, 0);
    assert_eq!(usage.non_cached_input_tokens, 50);
    assert_eq!(usage.visible_output_tokens(), 12);
}

#[test]
fn test_parse_usage_none() {
    let usage = parse_usage(None);
    assert_eq!(usage, houyicoder_protocol::llm::Usage::default());
}

/// An abnormal close (the byte stream ended without an explicit
/// finish_reason) must signal a mid-text cut when text was in flight, so the
/// engine's length-recovery re-calls for the tail rather than silently
/// accepting a truncated reply. When no text was generated, the close was
/// clean. Guards against a regression that reverts length back to stop.
#[test]
fn test_close_events_length_text() {
    let events = abnormal_close_events(false, true, Vec::new(), None);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, LlmEvent::Finish { reason, .. } if reason == "length")),
        "text in flight on an abnormal close must signal length for recovery: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, LlmEvent::TextEnd { .. })),
        "the open text block must be closed: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, LlmEvent::ReasoningEnd { .. })),
        "no reasoning end when reasoning never started: {events:?}"
    );
}

#[test]
fn test_close_stop_no_text() {
    let events = abnormal_close_events(false, false, Vec::new(), None);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, LlmEvent::Finish { reason, .. } if reason == "stop")),
        "an abnormal close with no text in flight is a clean stop: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, LlmEvent::TextEnd { .. })),
        "no text end when text never started: {events:?}"
    );
}

#[test]
fn test_close_events_close_reasoning() {
    let events = abnormal_close_events(true, true, Vec::new(), None);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, LlmEvent::ReasoningEnd { .. })),
        "an open reasoning block must be closed: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, LlmEvent::Finish { reason, .. } if reason == "length")),
        "text in flight still signals length: {events:?}"
    );
}

#[test]
fn test_finalize_mints_empty_id() {
    // Some OpenAI-compatible endpoints omit the id field on streamed tool_call
    // deltas. Without a minted id, every call shares an empty id and the
    // transcript's FIFO pairing crosses results across calls (a Read result
    // landing under an Edit chip). An empty id must be replaced with a unique
    // non-empty one; two empty-id calls get distinct ids.
    let mut acc: Vec<ToolCallAccum> = Vec::new();
    accumulate_tool_call(
        &mut acc,
        // No id field in the delta.
        &json!({"index": 0, "function": {"name": "read", "arguments": "{\"path\":\"a\"}"}}),
    );
    accumulate_tool_call(
        &mut acc,
        &json!({"index": 1, "function": {"name": "edit", "arguments": "{\"path\":\"a\"}"}}),
    );
    let events = finalize_tool_calls(acc);
    assert_eq!(events.len(), 2);
    let ids: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::ToolCall { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert!(ids.iter().all(|id| !id.is_empty()), "no empty id: {ids:?}");
    assert!(ids[0] != ids[1], "distinct minted ids: {ids:?}");
    assert!(ids[0].starts_with("houyi_tc_"), "minted prefix: {ids:?}");
}

#[test]
fn test_finalize_mints_duplicate_id() {
    // Two tool calls in one streamed response that share a genuine non-empty
    // id (a provider reusing one id across calls) must not both ship with the
    // same id: the second is minted so transcript pairing stays unambiguous.
    let mut acc: Vec<ToolCallAccum> = Vec::new();
    accumulate_tool_call(
        &mut acc,
        &json!({"index": 0, "id": "dup", "function": {"name": "read", "arguments": "{\"path\":\"a\"}"}}),
    );
    accumulate_tool_call(
        &mut acc,
        &json!({"index": 1, "id": "dup", "function": {"name": "edit", "arguments": "{\"path\":\"a\"}"}}),
    );
    let events = finalize_tool_calls(acc);
    assert_eq!(events.len(), 2);
    let ids: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::ToolCall { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids[0], "dup", "first call keeps its id: {ids:?}");
    assert!(ids[1].starts_with("houyi_tc_"), "duplicate minted: {ids:?}");
    assert_ne!(ids[0], ids[1], "distinct: {ids:?}");
}

#[test]
fn test_parse_mints_empty_id() {
    // Non-streaming path: two tool calls in one message both omit the id
    // field. Both must be minted distinct so they never share an empty id.
    let json = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "tool_calls": [
                    {"id": null, "type": "function", "function": {"name": "read", "arguments": "{}"}},
                    {"type": "function", "function": {"name": "edit", "arguments": "{}"}}
                ]
            }
        }],
    });
    let resp = parse_response(&json, "m").unwrap();
    let ids: Vec<&str> = resp
        .output
        .iter()
        .filter_map(|o| match o {
            OutputItem::ToolCall { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.iter().all(|id| !id.is_empty()), "no empty id: {ids:?}");
    assert_ne!(ids[0], ids[1], "distinct minted: {ids:?}");
    assert!(
        ids[0].starts_with("houyi_tc_") && ids[1].starts_with("houyi_tc_"),
        "minted prefix: {ids:?}"
    );
}

#[test]
fn test_parse_mints_duplicate_id() {
    // Non-streaming path: two tool calls share a genuine id. The second must
    // be minted so the transcript FIFO and approval routing do not cross them.
    let json = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "tool_calls": [
                    {"id": "dup", "type": "function", "function": {"name": "read", "arguments": "{}"}},
                    {"id": "dup", "type": "function", "function": {"name": "edit", "arguments": "{}"}}
                ]
            }
        }],
    });
    let resp = parse_response(&json, "m").unwrap();
    let ids: Vec<&str> = resp
        .output
        .iter()
        .filter_map(|o| match o {
            OutputItem::ToolCall { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], "dup", "first keeps its id: {ids:?}");
    assert!(ids[1].starts_with("houyi_tc_"), "duplicate minted: {ids:?}");
    assert_ne!(ids[0], ids[1], "distinct: {ids:?}");
}

// ---- effort dialect + request-body injection ----

fn req_with(model: &str, settings: ModelSettings) -> CompletionRequest {
    CompletionRequest {
        model: model.into(),
        instructions: "x".into(),
        input: vec![InputItem::User {
            content: "hi".into(),
        }],
        tools: vec![],
        settings,
        cache_breakpoints: Vec::new(),
    }
}

#[test]
fn test_effort_dialect_paths() {
    assert_eq!(effort_dialect("qwen3.7-max"), EffortDialect::Qwen3);
    assert_eq!(effort_dialect("QWEN3-CODER"), EffortDialect::Qwen3);
    assert_eq!(effort_dialect("o3-mini"), EffortDialect::OpenaiReasoning);
    assert_eq!(effort_dialect("o1"), EffortDialect::OpenaiReasoning);
    assert_eq!(effort_dialect("gpt-5"), EffortDialect::OpenaiReasoning);
    assert_eq!(effort_dialect("deepseek-chat"), EffortDialect::NotSupported);
    assert_eq!(effort_dialect("glm-5.2"), EffortDialect::NotSupported);
    assert_eq!(effort_dialect("qwen3-reasoning"), EffortDialect::Qwen3);
}

#[test]
fn test_body_emits_reasoning() {
    let r = req_with(
        "o3-mini",
        ModelSettings {
            reasoning_effort: Some(EffortLevel::Medium),
            max_output_tokens: Some(8000),
            ..Default::default()
        },
    );
    let body = build_request_body(&r);
    assert_eq!(body["reasoning_effort"], json!("medium"));
    assert!(
        body.get("enable_thinking").is_none(),
        "no thinking flag on reasoning arm"
    );
    assert!(
        body.get("thinking_budget").is_none(),
        "no thinking budget on reasoning arm"
    );
}

#[test]
fn test_body_emits_qwen() {
    let r = req_with(
        "qwen3.7-max",
        ModelSettings {
            enable_thinking: Some(true),
            thinking_budget: Some(8192),
            max_output_tokens: Some(32768),
            ..Default::default()
        },
    );
    let body = build_request_body(&r);
    assert_eq!(body["enable_thinking"], json!(true));
    assert_eq!(body["thinking_budget"], json!(8192));
    assert!(
        body.get("reasoning_effort").is_none(),
        "no reasoning_effort on qwen arm"
    );
}

#[test]
fn test_body_omits_budget_low() {
    // Low => enable_thinking false. A budget alongside a false flag is a
    // contradictory request, so the budget is suppressed even when set.
    let r = req_with(
        "qwen3.7-max",
        ModelSettings {
            enable_thinking: Some(false),
            thinking_budget: Some(8192),
            max_output_tokens: Some(32768),
            ..Default::default()
        },
    );
    let body = build_request_body(&r);
    assert_eq!(body["enable_thinking"], json!(false));
    assert!(
        body.get("thinking_budget").is_none(),
        "budget must not ship when thinking is off"
    );
}

#[test]
fn test_body_omits_unsupported() {
    // A model in neither family sends no effort fields, even if the settings
    // struct carries values (a stale/misconfigured caller cannot leak them).
    let r = req_with(
        "deepseek-chat",
        ModelSettings {
            reasoning_effort: Some(EffortLevel::High),
            enable_thinking: Some(true),
            thinking_budget: Some(8192),
            max_output_tokens: Some(32768),
            ..Default::default()
        },
    );
    let body = build_request_body(&r);
    assert!(body.get("reasoning_effort").is_none());
    assert!(body.get("enable_thinking").is_none());
    assert!(body.get("thinking_budget").is_none());
}

#[test]
fn test_budget_clamps() {
    // budget == max_output_tokens would equal the output room; clamp to cap-1
    // so thinking never asks for more than the total output allows.
    let r = req_with(
        "qwen3.7-max",
        ModelSettings {
            enable_thinking: Some(true),
            thinking_budget: Some(8192),
            max_output_tokens: Some(8192),
            ..Default::default()
        },
    );
    let body = build_request_body(&r);
    assert_eq!(body["thinking_budget"], json!(8191));
}
