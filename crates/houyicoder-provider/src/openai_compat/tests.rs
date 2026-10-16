use super::*;
use crate::http_error::{classify_status, classify_with_body, parse_retry_after};
use houyicoder_protocol::cache_policy::{
    BreakpointKind, CacheBreakpoint, CacheHint, CacheTtl, PromptCacheFormat,
};
use houyicoder_protocol::llm::{AssistantToolCall, EffortLevel, ModelSettings, SpeedMode, ToolDef};
use houyicoder_protocol::llm::{Usage, UsageProvenance};
use reqwest::header::HeaderMap;

fn req(input: Vec<InputItem>, tools: Vec<ToolDef>) -> CompletionRequest {
    CompletionRequest {
        model: "test-model".into(),
        instructions: "you are a test".into(),
        dynamic_boundary: None,
        input,
        tools,
        settings: ModelSettings {
            max_output_tokens: Some(8000),
            temperature: Some(0.0),
            ..Default::default()
        },
        cache_breakpoints: Vec::new(),
        cache_format: PromptCacheFormat::AutoPrefix,
    }
}

/// The cache breakpoints the Auto policy places.
fn auto_breakpoints() -> Vec<CacheBreakpoint> {
    let hint = CacheHint::Ephemeral(CacheTtl::OneHour);
    [
        BreakpointKind::SystemStaticPrefix,
        BreakpointKind::LastToolDef,
        BreakpointKind::LatestUserMessage,
    ]
    .into_iter()
    .map(|kind| CacheBreakpoint {
        kind,
        hint: hint.clone(),
    })
    .collect()
}

/// How many cache markers the payload carries. Counted over the whole
/// serialized body, so a marker at an unexpected position is counted too.
fn marker_count(body: &Value) -> usize {
    body.to_string().matches("cache_control").count()
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

/// Fast lowers to the OpenAI service_tier request field; Standard and an
/// unset tier leave the field absent so the project default stands.
#[test]
fn test_service_tier_lowering() {
    let base = || {
        req(
            vec![InputItem::User {
                content: "hi".into(),
            }],
            vec![],
        )
    };

    let mut fast = base();
    fast.settings.speed = Some(SpeedMode::Fast);
    let body = build_request_body(&fast);
    assert_eq!(body["service_tier"], "fast", "fast tier is sent");

    let standard = base();
    let body = build_request_body(&standard);
    assert!(
        body.get("service_tier").is_none(),
        "unset tier sends no field: {body}"
    );
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

/// The SystemStaticPrefix cache breakpoint lowers to a prompt_cache_key so the
/// endpoint's auto-cache reuses the stable system+tools prefix across requests,
/// and the automatic-prefix form sends no marker. Without a breakpoint (the
/// None policy), no key is sent.
#[test]
fn test_prompt_cache_key_lowered() {
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
    assert!(
        body["messages"][0]["content"].is_string(),
        "the automatic-prefix form sends the instructions as plain text: {body}"
    );
    assert_eq!(
        marker_count(&body),
        0,
        "the automatic-prefix form sends no marker: {body}"
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

/// A declared marker-bearing endpoint gets each placed breakpoint at its own
/// position — the system prefix, the last tool definition, the newest message
/// carrying text — and no cache label, since the label means nothing there.
#[test]
fn test_positional_markers_lowered() {
    let mut r = req(
        vec![
            InputItem::User {
                content: "first".into(),
            },
            InputItem::Assistant {
                content: "let me look".into(),
                tool_calls: vec![],
            },
            InputItem::ToolResult {
                call_id: "c1".into(),
                output: json!({"ok": true}),
            },
        ],
        vec![
            ToolDef {
                name: "edit".into(),
                description: "edit a file".into(),
                input_schema: json!({"type": "object"}),
            },
            ToolDef {
                name: "glob".into(),
                description: "find files".into(),
                input_schema: json!({"type": "object"}),
            },
        ],
    );
    r.cache_format = PromptCacheFormat::PositionalMarkers;
    r.cache_breakpoints = auto_breakpoints();

    let body = build_request_body(&r);
    assert_eq!(
        marker_count(&body),
        3,
        "one marker per placed breakpoint: {body}"
    );
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral",
        "the system prefix is marked"
    );
    assert!(
        body["tools"][0].get("cache_control").is_none(),
        "only the last tool definition is marked: {body}"
    );
    assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
    let tail = body["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(tail["role"], "tool");
    assert_eq!(
        tail["content"][0]["cache_control"]["type"], "ephemeral",
        "the newest message is marked"
    );
    assert_eq!(
        body["messages"][1]["content"], "first",
        "an earlier message stays plain"
    );
    assert!(
        body.get("prompt_cache_key").is_none(),
        "the marker form sends no cache label: {body}"
    );
}

/// The two axes are independent: the declared form says how a position is
/// expressed, the placed breakpoints say which positions exist, so a
/// breakpoint the policy did not place stays unmarked.
#[test]
fn test_markers_follow_policy() {
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
    r.cache_format = PromptCacheFormat::PositionalMarkers;
    r.cache_breakpoints = vec![CacheBreakpoint {
        kind: BreakpointKind::SystemStaticPrefix,
        hint: CacheHint::Ephemeral(CacheTtl::OneHour),
    }];

    let body = build_request_body(&r);
    assert_eq!(
        marker_count(&body),
        1,
        "only the placed breakpoint is marked: {body}"
    );
    assert!(body["tools"][0].get("cache_control").is_none());
}

/// A tail message with no text to mark cannot carry the marker, so the scan
/// moves to the newest message that can instead of dropping it.
#[test]
fn test_marker_skips_empty() {
    let mut r = req(
        vec![
            InputItem::ToolResult {
                call_id: "c1".into(),
                output: json!({"ok": true}),
            },
            InputItem::Assistant {
                content: String::new(),
                tool_calls: vec![AssistantToolCall {
                    id: "c2".into(),
                    name: "edit".into(),
                    input: json!({}),
                }],
            },
        ],
        vec![],
    );
    r.cache_format = PromptCacheFormat::PositionalMarkers;
    r.cache_breakpoints = vec![CacheBreakpoint {
        kind: BreakpointKind::LatestUserMessage,
        hint: CacheHint::Ephemeral(CacheTtl::OneHour),
    }];

    let body = build_request_body(&r);
    assert_eq!(
        marker_count(&body),
        1,
        "the marker lands on the newest message with text: {body}"
    );
    assert_eq!(body["messages"][1]["role"], "tool");
    assert_eq!(
        body["messages"][1]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert!(
        body["messages"][2]["content"].is_string(),
        "the textless tail keeps its plain content: {body}"
    );
}

/// A system prompt with no text cannot carry a marker, so the plain string
/// form stays rather than a marker over nothing.
#[test]
fn test_empty_system_stays_plain() {
    let mut r = req(
        vec![InputItem::User {
            content: "hi".into(),
        }],
        vec![],
    );
    r.instructions = String::new();
    r.cache_format = PromptCacheFormat::PositionalMarkers;
    r.cache_breakpoints = vec![CacheBreakpoint {
        kind: BreakpointKind::SystemStaticPrefix,
        hint: CacheHint::Ephemeral(CacheTtl::OneHour),
    }];

    let body = build_request_body(&r);
    assert_eq!(
        marker_count(&body),
        0,
        "no marker over an empty system prompt: {body}"
    );
    assert!(body["messages"][0]["content"].is_string());
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

/// The cache key hashes only the static segment before dynamic_boundary, so two
/// requests that share a static prefix but differ in the cwd-dependent dynamic
/// suffix produce the same key. Before the offset reached the provider the key
/// hashed the whole instructions and rotated on every cwd change. This asserts
/// the offline key stability only: whether the endpoint then returns a cache
/// hit depends on the request-body order and the usage the endpoint reports.
/// The contrast at the end shows the boundary, not a coincidence, makes the two
/// equal, since with no offset threaded the differing suffixes rotate the key.
#[test]
fn test_cache_key_excludes_dynamic() {
    let static_segment = "you are a test";
    let bp = vec![CacheBreakpoint {
        kind: BreakpointKind::SystemStaticPrefix,
        hint: CacheHint::Ephemeral(CacheTtl::OneHour),
    }];
    let tools = vec![ToolDef {
        name: "edit".into(),
        description: "edit a file".into(),
        input_schema: json!({"type": "object"}),
    }];
    // Same static segment, two different working directories in the suffix.
    let mut a = req(
        vec![InputItem::User {
            content: "hi".into(),
        }],
        tools.clone(),
    );
    a.instructions = format!("{static_segment}\n\ncwd: /workspace/a");
    a.dynamic_boundary = Some(static_segment.len());
    a.cache_breakpoints = bp.clone();
    let mut b = req(
        vec![InputItem::User {
            content: "hi".into(),
        }],
        tools.clone(),
    );
    b.instructions = format!("{static_segment}\n\ncwd: /workspace/b");
    b.dynamic_boundary = Some(static_segment.len());
    b.cache_breakpoints = bp.clone();

    let key_a = build_request_body(&a)["prompt_cache_key"]
        .as_str()
        .expect("key lowered")
        .to_string();
    let key_b = build_request_body(&b)["prompt_cache_key"]
        .as_str()
        .expect("key lowered")
        .to_string();
    assert_eq!(
        key_a, key_b,
        "a cwd change confined to the dynamic suffix keeps the key stable"
    );

    // Contrast: with no boundary threaded, the whole instructions is the static
    // segment, so the differing suffixes rotate the key. This is the behavior
    // the boundary fixes, and it proves the equality above is not trivial.
    a.dynamic_boundary = None;
    b.dynamic_boundary = None;
    let key_a_unbounded = build_request_body(&a)["prompt_cache_key"]
        .as_str()
        .expect("key lowered")
        .to_string();
    let key_b_unbounded = build_request_body(&b)["prompt_cache_key"]
        .as_str()
        .expect("key lowered")
        .to_string();
    assert_ne!(
        key_a_unbounded, key_b_unbounded,
        "without the boundary the dynamic suffix enters the key"
    );
}

/// Flatten a request body into the contiguous content the provider renders, in
/// send order: the leading system message, then the tool definitions, then the
/// rest of the messages. Read from build_request_body's output so the model
/// tracks the real body; the system-before-tools order is a client-side model of
/// the chat template's render, not the provider's internal truth. Faithful enough
/// to locate the byte where two bodies' prefixes first diverge, which is where
/// the cache stops reusing.
fn send_order_content(body: &Value) -> String {
    let mut out = String::new();
    let messages = body["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if let Some(system) = messages.first() {
        push_rendered(&mut out, system);
    }
    for tool in body["tools"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        out.push_str(&tool.to_string());
    }
    for message in messages.iter().skip(1) {
        push_rendered(&mut out, message);
    }
    out
}

/// Append one message's rendered text: its content, a plain string or a list of
/// text blocks, then any tool calls, in body order.
fn push_rendered(out: &mut String, message: &Value) {
    match &message["content"] {
        Value::String(text) => out.push_str(text),
        Value::Array(blocks) => {
            for block in blocks {
                if let Some(text) = block["text"].as_str() {
                    out.push_str(text);
                }
            }
        }
        _ => {}
    }
    for call in message["tool_calls"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        out.push_str(&call.to_string());
    }
}

/// The byte offset where two content sequences first differ, or the length of
/// the shorter when one is a prefix of the other. The provider's cache reuses
/// the bytes before this offset and recomputes everything from it onward.
fn first_divergence(a: &str, b: &str) -> usize {
    a.bytes()
        .zip(b.bytes())
        .position(|(x, y)| x != y)
        .unwrap_or_else(|| a.len().min(b.len()))
}

/// A cwd change confined to the dynamic suffix keeps the cache key stable, as
/// test_cache_key_excludes_dynamic asserts, but it still diverges the contiguous
/// prefix the provider actually renders. Both requests carry byte-identical tool
/// definitions, yet the tools sit after the system content, so the prefix
/// diverges inside the system message and the identical tools past that point do
/// not reuse. Equal fields do not make an equal contiguous prefix. This is the
/// offline evidence for accepting cross-cwd reuse of the static segment only;
/// confirming the endpoint returns those cached tokens needs a live measurement.
#[test]
fn test_prefix_diverges_before_tools() {
    let static_segment = "you are a test";
    let boundary = static_segment.len();
    let tools = vec![ToolDef {
        name: "edit".into(),
        description: "edit a file".into(),
        input_schema: json!({"type": "object"}),
    }];
    let bp = vec![CacheBreakpoint {
        kind: BreakpointKind::SystemStaticPrefix,
        hint: CacheHint::Ephemeral(CacheTtl::OneHour),
    }];
    let history = vec![InputItem::User {
        content: "hi".into(),
    }];
    let mut a = req(history.clone(), tools.clone());
    a.instructions = format!("{static_segment}\n\n# Project context\n\nAlpha: use spaces.");
    a.dynamic_boundary = Some(boundary);
    a.cache_breakpoints = bp.clone();
    let mut b = req(history.clone(), tools.clone());
    b.instructions = format!("{static_segment}\n\n# Project context\n\nBeta: use tabs.");
    b.dynamic_boundary = Some(boundary);
    b.cache_breakpoints = bp.clone();

    let body_a = build_request_body(&a);
    let seq_a = send_order_content(&body_a);
    let seq_b = send_order_content(&build_request_body(&b));
    let div = first_divergence(&seq_a, &seq_b);

    // The static segment is identical across the cwd change, so the divergence
    // cannot precede it: at least the static segment reuses.
    assert!(
        div >= boundary,
        "the static segment survives a cwd change (divergence {div} < boundary {boundary})"
    );
    assert!(
        seq_a[..div].starts_with(static_segment),
        "the reusable prefix begins with the whole static segment"
    );
    // The divergence falls inside the system content, before the tools and
    // history that render after it.
    assert!(
        div < a.instructions.len(),
        "the divergence is inside the system content ({} < {})",
        div,
        a.instructions.len()
    );
    // The tool definitions are byte-identical in both bodies...
    let tools_render: String = body_a["tools"]
        .as_array()
        .expect("tools lowered into the body")
        .iter()
        .map(|t| t.to_string())
        .collect();
    assert!(
        seq_a.contains(&tools_render) && seq_b.contains(&tools_render),
        "the tool definitions are byte-identical in both bodies"
    );
    // ...yet they sit past the divergence, outside the reusable contiguous
    // prefix: identical fields that still do not cache across the cwd change.
    assert!(
        !seq_a[..div].contains(&tools_render),
        "the identical tools sit past the divergence, so they do not reuse across cwd"
    );
}

/// Within a session the cwd is fixed, so the system message and tools are
/// byte-identical turn to turn and the only change is history appended at the
/// tail. The contiguous prefix therefore diverges exactly at the end of the prior
/// turn: the whole system + tools + prior history reuses and only the appended
/// tail is new. This steady-state append is what drives the hit-rate target, and
/// it needs no reorder — the order accepted for the cross-cwd case costs nothing
/// on this common path.
#[test]
fn test_append_prefix_reuses() {
    let static_segment = "you are a test";
    let instructions = format!("{static_segment}\n\n# Project context\n\nAlpha: use spaces.");
    let tools = vec![ToolDef {
        name: "edit".into(),
        description: "edit a file".into(),
        input_schema: json!({"type": "object"}),
    }];
    let turn1 = vec![InputItem::User {
        content: "first".into(),
    }];
    let turn2 = vec![
        InputItem::User {
            content: "first".into(),
        },
        InputItem::Assistant {
            content: "reply".into(),
            tool_calls: vec![],
        },
        InputItem::User {
            content: "second".into(),
        },
    ];
    let mut a = req(turn1, tools.clone());
    a.instructions = instructions.clone();
    let mut b = req(turn2, tools.clone());
    b.instructions = instructions.clone();

    let seq_a = send_order_content(&build_request_body(&a));
    let seq_b = send_order_content(&build_request_body(&b));
    let div = first_divergence(&seq_a, &seq_b);

    // Turn 1 is a contiguous prefix of turn 2: the divergence is at the very end
    // of turn 1, so the whole prior prefix reuses and only the appended tail is
    // recomputed.
    assert_eq!(
        div,
        seq_a.len(),
        "an appended turn diverges at the tail, not mid-prefix"
    );
    assert!(
        seq_b.starts_with(&seq_a),
        "the prior turn is a contiguous prefix of the next"
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
    let mut h = HeaderMap::new();
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
    // No cached_tokens key: input is reported but the cache-read count is not,
    // so the hit rate is unknown rather than a misleading 0%.
    assert_eq!(
        usage.provenance,
        UsageProvenance {
            input_reported: true,
            cache_read_reported: false,
            cache_write_reported: false,
        }
    );
    assert_eq!(usage.cache_hit_rate(), None);
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
    // prompt_tokens and cached_tokens both reported: the account is complete,
    // so the hit rate is a real number. The subset model has no separate
    // cache-write count, so write stays unreported.
    assert_eq!(
        usage.provenance,
        UsageProvenance {
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: false,
        }
    );
    assert_eq!(usage.cache_hit_rate(), Some(0.7));
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
    assert!(
        !usage.provenance.cache_read_reported,
        "no cached key means the cache-read count is unreported, not a confirmed zero"
    );
}

#[test]
fn test_parse_usage_none() {
    let usage = parse_usage(None);
    assert_eq!(usage, Usage::default());
    assert_eq!(usage.provenance, UsageProvenance::default());
    assert_eq!(
        usage.cache_hit_rate(),
        None,
        "no usage object: unknown, not 0%"
    );
}

/// DeepSeek reports prompt_cache_hit_tokens and prompt_cache_miss_tokens at
/// the usage root; cached_tokens may appear under prompt_tokens_details
/// alongside them. When only the root-level fields are present, the parser
/// falls back to prompt_cache_hit_tokens for the cache-read count, and
/// non_cached is the miss remainder so the identity holds.
#[test]
fn test_parse_usage_deepseek_fold() {
    let u = json!({
        "prompt_tokens": 1000,
        "completion_tokens": 40,
        "total_tokens": 1040,
        "prompt_cache_hit_tokens": 700,
        "prompt_cache_miss_tokens": 300
    });
    let usage = parse_usage(Some(&u));
    assert_eq!(usage.input_tokens, 1000);
    assert_eq!(usage.cache_read_input_tokens, 700);
    assert_eq!(usage.non_cached_input_tokens, 300, "1000 - 700 cached");
    // Material identity, not a constant-true fallback.
    assert_eq!(
        usage.non_cached_input_tokens as u64
            + usage.cache_read_input_tokens as u64
            + usage.cache_write_input_tokens as u64,
        usage.input_tokens as u64,
        "non_cached + cache_read + cache_write = input"
    );
    // cache_read parsed from the DeepSeek fallback key.
    assert_eq!(
        usage.provenance,
        UsageProvenance {
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: false,
        }
    );
    assert_eq!(usage.cache_hit_rate(), Some(0.7));
}

/// When a gateway relays Anthropic's cache_creation_input_tokens through the
/// usage object, cache_write is parsed and subtracted from non_cached so the
/// identity holds. Without it, non_cached would silently absorb the write.
#[test]
fn test_parse_usage_cache_creation() {
    let u = json!({
        "prompt_tokens": 3000,
        "completion_tokens": 80,
        "total_tokens": 3080,
        "prompt_tokens_details": {"cached_tokens": 1800},
        "cache_creation_input_tokens": 200
    });
    let usage = parse_usage(Some(&u));
    assert_eq!(usage.input_tokens, 3000);
    assert_eq!(usage.cache_read_input_tokens, 1800);
    assert_eq!(usage.cache_write_input_tokens, 200);
    // non_cached = input - cache_read - cache_write
    assert_eq!(usage.non_cached_input_tokens, 1000);
    assert_eq!(
        usage.non_cached_input_tokens as u64
            + usage.cache_read_input_tokens as u64
            + usage.cache_write_input_tokens as u64,
        usage.input_tokens as u64,
        "non_cached + cache_read + cache_write = input"
    );
    assert_eq!(
        usage.provenance,
        UsageProvenance {
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: true,
        }
    );
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
        dynamic_boundary: None,
        input: vec![InputItem::User {
            content: "hi".into(),
        }],
        tools: vec![],
        settings,
        cache_breakpoints: Vec::new(),
        cache_format: PromptCacheFormat::AutoPrefix,
    }
}

#[test]
fn test_effort_dialect_paths() {
    assert_eq!(effort_dialect("qwen3.7-max"), EffortDialect::Qwen3);
    assert_eq!(effort_dialect("QWEN3-CODER"), EffortDialect::Qwen3);
    assert_eq!(effort_dialect("gpt-5"), EffortDialect::OpenaiReasoning);
    assert_eq!(effort_dialect("deepseek-chat"), EffortDialect::NotSupported);
    assert_eq!(
        effort_dialect("deepseek-v4-pro"),
        EffortDialect::OpenaiReasoning
    );
    assert_eq!(
        effort_dialect("deepseek-v4-pro-0813"),
        EffortDialect::OpenaiReasoning
    );
    assert_eq!(effort_dialect("glm-5.2"), EffortDialect::Glm);
    assert_eq!(effort_dialect("glm-5.3"), EffortDialect::Glm);
    assert_eq!(effort_dialect("glm-5.1"), EffortDialect::NotSupported);
    assert_eq!(effort_dialect("qwen3-reasoning"), EffortDialect::Qwen3);
}

#[test]
fn test_body_emits_reasoning() {
    let r = req_with(
        "gpt-5.6",
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
        "no thinking flag on the reasoning branch"
    );
    assert!(
        body.get("thinking_budget").is_none(),
        "no thinking budget on the reasoning branch"
    );
}

#[test]
fn test_body_emits_deepseek_reasoning() {
    let r = req_with(
        "deepseek-v4-pro",
        ModelSettings {
            reasoning_effort: Some(EffortLevel::Max),
            max_output_tokens: Some(8000),
            ..Default::default()
        },
    );
    let body = build_request_body(&r);
    assert_eq!(body["reasoning_effort"], json!("max"));
    assert!(
        body.get("enable_thinking").is_none(),
        "no thinking flag on the deepseek reasoning branch"
    );
    assert!(
        body.get("thinking_budget").is_none(),
        "no thinking budget on the deepseek reasoning branch"
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
        "no reasoning_effort on the qwen branch"
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
