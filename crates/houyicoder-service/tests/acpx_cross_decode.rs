//! Cross-decode fidelity: the protocol layer SessionUpdate copy decodes the
//! exact JSON the agent-client-protocol crate emits, and vice versa.
//! This is the ACP-fidelity gate: a peer using the real ACP SDK produces JSON
//! our protocol layer accepts, and our protocol layer produces JSON a peer with
//! the real ACP SDK accepts.
//!
//! Semantic comparison only — our copy intentionally omits optional
//! fields (annotations, uri, message_id, the monetary cost on usage updates,
//! _meta) and skips default-valued fields, so bit-equality would fail; the
//! fields we both carry must match. Each variant is tested in both directions
//! (ACP to ours, ours to ACP).
//!
//! The SDK reorganized its schema types under schema::v1 in the 2.0 release;
//! the v1 module is the stable wire this gate targets.
//!
//! Feature-gated: the agent-client-protocol dev-dep pulls the schema crate
//! and its serde machinery, whose cold compile pushes the workspace
//! test-compile past the 120s gate. Run with the acp-cross-decode feature
//! enabled; the default test pass skips this file so the gate stays fast. The
//! fidelity gate runs when the protocol mirror changes or on CI, not on every
//! commit.

#![cfg(feature = "acp-cross-decode")]

use agent_client_protocol::schema::v1 as acp;
use houyicoder_protocol::frontend::run::ContentBlock as OurContentBlock;
use houyicoder_protocol::frontend::session_update::{
    ContentChunk as OurContentChunk, SessionUpdate as OurSessionUpdate, ToolCall as OurToolCall,
    ToolCallStatus as OurToolCallStatus, ToolCallUpdate as OurToolCallUpdate,
    ToolCallUpdateFields as OurToolCallUpdateFields, UsageUpdate as OurUsageUpdate,
};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Serialize an ACP value, decode into our mirror, and return the parsed
/// ours-side value. Panics on any serde failure — the two shapes must be
/// compatible.
fn acp_to_ours(acp_val: &acp::SessionUpdate) -> OurSessionUpdate {
    let json = serde_json::to_string(acp_val).expect("ACP serialize");
    serde_json::from_str(&json).expect("decode into ours")
}

/// Serialize one of our values, decode into the ACP type, and return the
/// parsed ACP value. Panics on any serde failure.
fn ours_to_acp(ours: &OurSessionUpdate) -> acp::SessionUpdate {
    let json = serde_json::to_string(ours).expect("ours serialize");
    serde_json::from_str(&json).expect("decode into ACP")
}

// ---------------------------------------------------------------------------
// AgentMessageChunk — Text content block
// ---------------------------------------------------------------------------

#[test]
fn test_acp_ours_agent_text() {
    let acp_val = acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
        acp::ContentBlock::Text(acp::TextContent::new("hello")),
    ));
    let ours = acp_to_ours(&acp_val);
    match &ours {
        OurSessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
            OurContentBlock::Text { text } => assert_eq!(text, "hello"),
            _ => panic!("expected text content block, got {:?}", chunk.content),
        },
        _ => panic!("expected AgentMessageChunk, got {ours:?}"),
    }
}

#[test]
fn test_ours_acp_agent_text() {
    let ours = OurSessionUpdate::AgentMessageChunk(OurContentChunk::new(OurContentBlock::Text {
        text: "hello".into(),
    }));
    let acp_val = ours_to_acp(&ours);
    match acp_val {
        acp::SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
            acp::ContentBlock::Text(tc) => assert_eq!(tc.text, "hello"),
            _ => panic!("expected text content block"),
        },
        _ => panic!("expected AgentMessageChunk"),
    }
}

// ---------------------------------------------------------------------------
// UserMessageChunk + AgentThoughtChunk — same shape, different tag
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// AgentMessageChunk — Image content block (camelCase mimeType fidelity)
// ---------------------------------------------------------------------------

#[test]
fn test_acp_ours_agent_image() {
    // ACP ImageContent emits data + mimeType (camelCase). Our mirror must
    // decode mimeType into mime_type; this regressed before the field rename.
    let acp_val = acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
        acp::ContentBlock::Image(acp::ImageContent::new("b64", "image/png")),
    ));
    let ours = acp_to_ours(&acp_val);
    match &ours {
        OurSessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
            OurContentBlock::Image { data, mime_type } => {
                assert_eq!(data, "b64");
                assert_eq!(mime_type, "image/png");
            }
            _ => panic!("expected image content block, got {:?}", chunk.content),
        },
        _ => panic!("expected AgentMessageChunk, got {ours:?}"),
    }
}

#[test]
fn test_ours_acp_agent_image() {
    let ours = OurSessionUpdate::AgentMessageChunk(OurContentChunk::new(OurContentBlock::Image {
        data: "b64".into(),
        mime_type: "image/png".into(),
    }));
    let acp_val = ours_to_acp(&ours);
    match acp_val {
        acp::SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
            acp::ContentBlock::Image(img) => {
                assert_eq!(img.data, "b64");
                assert_eq!(img.mime_type, "image/png");
            }
            _ => panic!("expected image content block"),
        },
        _ => panic!("expected AgentMessageChunk"),
    }
}

#[test]
fn test_acp_ours_user_text() {
    let acp_val = acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(
        acp::ContentBlock::Text(acp::TextContent::new("user says")),
    ));
    let ours = acp_to_ours(&acp_val);
    match &ours {
        OurSessionUpdate::UserMessageChunk(chunk) => match &chunk.content {
            OurContentBlock::Text { text } => assert_eq!(text, "user says"),
            _ => panic!("expected text content block"),
        },
        _ => panic!("expected UserMessageChunk, got {ours:?}"),
    }
}

#[test]
fn test_ours_acp_thought_text() {
    let ours = OurSessionUpdate::AgentThoughtChunk(OurContentChunk::new(OurContentBlock::Text {
        text: "thinking".into(),
    }));
    let acp_val = ours_to_acp(&ours);
    match acp_val {
        acp::SessionUpdate::AgentThoughtChunk(chunk) => match chunk.content {
            acp::ContentBlock::Text(tc) => assert_eq!(tc.text, "thinking"),
            _ => panic!("expected text content block"),
        },
        _ => panic!("expected AgentThoughtChunk"),
    }
}

// ---------------------------------------------------------------------------
// ToolCall — tool_call_id + title + raw_input + status
// ---------------------------------------------------------------------------

#[test]
fn test_acp_ours_tool_call() {
    let acp_val = acp::SessionUpdate::ToolCall(
        acp::ToolCall::new(acp::ToolCallId::new("toolu_1"), "bash")
            .raw_input(serde_json::json!("ls"))
            .status(acp::ToolCallStatus::InProgress),
    );
    let ours = acp_to_ours(&acp_val);
    match &ours {
        OurSessionUpdate::ToolCall(tc) => {
            assert_eq!(tc.tool_call_id.0, "toolu_1", "tool_call_id");
            assert_eq!(tc.title, "bash", "title");
            assert_eq!(tc.status, OurToolCallStatus::InProgress, "status");
            assert_eq!(tc.raw_input, Some(Value::String("ls".into())), "raw_input");
        }
        _ => panic!("expected ToolCall, got {ours:?}"),
    }
}

#[test]
fn test_ours_acp_tool_call() {
    let ours = OurSessionUpdate::ToolCall(
        OurToolCall::new("toolu_1", "bash")
            .raw_input(Value::String("ls".into()))
            .status(OurToolCallStatus::InProgress),
    );
    let acp_val = ours_to_acp(&ours);
    match acp_val {
        acp::SessionUpdate::ToolCall(tc) => {
            assert_eq!(tc.tool_call_id.0.as_ref(), "toolu_1", "tool_call_id");
            assert_eq!(tc.title, "bash", "title");
            assert_eq!(tc.status, acp::ToolCallStatus::InProgress, "status");
            assert_eq!(tc.raw_input, Some(serde_json::json!("ls")), "raw_input");
        }
        _ => panic!("expected ToolCall"),
    }
}

// ---------------------------------------------------------------------------
// ToolCallUpdate — status=Completed + raw_output (flattened fields)
// ---------------------------------------------------------------------------

#[test]
fn test_acp_ours_tool_update() {
    let acp_val = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
        acp::ToolCallId::new("toolu_1"),
        acp::ToolCallUpdateFields::new()
            .status(acp::ToolCallStatus::Completed)
            .raw_output(serde_json::json!("ok")),
    ));
    let ours = acp_to_ours(&acp_val);
    match &ours {
        OurSessionUpdate::ToolCallUpdate(upd) => {
            assert_eq!(upd.tool_call_id.0, "toolu_1", "tool_call_id");
            assert_eq!(
                upd.fields.status,
                Some(OurToolCallStatus::Completed),
                "status"
            );
            assert_eq!(
                upd.fields.raw_output,
                Some(Value::String("ok".into())),
                "raw_output"
            );
            // Fields are flattened, not nested under a fields key.
            let json = serde_json::to_string(&ours).expect("reserialize");
            assert!(
                !json.contains(r#""fields""#),
                "fields must be flattened: {json}"
            );
        }
        _ => panic!("expected ToolCallUpdate, got {ours:?}"),
    }
}

#[test]
fn test_ours_acp_tool_update() {
    let ours = OurSessionUpdate::ToolCallUpdate(OurToolCallUpdate::new(
        "toolu_1",
        OurToolCallUpdateFields::new()
            .status(OurToolCallStatus::Completed)
            .raw_output(Value::String("ok".into())),
    ));
    let acp_val = ours_to_acp(&ours);
    match acp_val {
        acp::SessionUpdate::ToolCallUpdate(upd) => {
            assert_eq!(upd.tool_call_id.0.as_ref(), "toolu_1", "tool_call_id");
            assert_eq!(
                upd.fields.status,
                Some(acp::ToolCallStatus::Completed),
                "status"
            );
            assert_eq!(
                upd.fields.raw_output,
                Some(serde_json::json!("ok")),
                "raw_output"
            );
        }
        _ => panic!("expected ToolCallUpdate"),
    }
}

// ---------------------------------------------------------------------------
// Unstable message_id is ignored on decode
// ---------------------------------------------------------------------------

#[test]
fn test_ignores_acp_message_id() {
    // ACP ContentChunk carries an optional message_id field. Our mirror omits
    // it; serde must ignore the extra key so decode parity holds.
    let acp_val = acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new("chunk")))
            .message_id("msg-uuid-1"),
    );
    let json = serde_json::to_string(&acp_val).expect("ACP serialize");
    assert!(
        json.contains(r#""messageId":"msg-uuid-1""#),
        "ACP must emit messageId with unstable feature: {json}"
    );
    let ours: OurSessionUpdate = serde_json::from_str(&json).expect("decode ignores messageId");
    match &ours {
        OurSessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
            OurContentBlock::Text { text } => assert_eq!(text, "chunk"),
            _ => panic!("expected text content block"),
        },
        _ => panic!("expected AgentMessageChunk, got {ours:?}"),
    }
    // Re-serializing ours must NOT carry messageId — we never emit it.
    let our_json = serde_json::to_string(&ours).expect("reserialize");
    assert!(
        !our_json.contains("messageId"),
        "ours must not emit messageId: {our_json}"
    );
}

// ---------------------------------------------------------------------------
// _meta and annotations are ignored on decode
// ---------------------------------------------------------------------------

#[test]
fn test_ignores_acp_meta_annotations() {
    // ACP TextContent may carry annotations and _meta. Our mirror omits
    // both; serde must ignore them so decode parity holds.
    let json = r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi","annotations":{"priority":0.5},"_meta":{"x":1}}}"#;
    let ours: OurSessionUpdate = serde_json::from_str(json).expect("decode peer shape");
    match &ours {
        OurSessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
            OurContentBlock::Text { text } => assert_eq!(text, "hi"),
            _ => panic!("expected text content block"),
        },
        _ => panic!("expected AgentMessageChunk, got {ours:?}"),
    }
}

// ---------------------------------------------------------------------------
// UsageUpdate — used + size (monetary cost ignored on decode)
// ---------------------------------------------------------------------------

#[test]
fn test_acp_ours_usage_update() {
    let acp_val = acp::SessionUpdate::UsageUpdate(acp::UsageUpdate::new(1200, 200_000));
    let ours = acp_to_ours(&acp_val);
    match &ours {
        OurSessionUpdate::UsageUpdate(usage) => {
            assert_eq!(usage.used, 1200, "used");
            assert_eq!(usage.size, 200_000, "size");
        }
        _ => panic!("expected UsageUpdate, got {ours:?}"),
    }
}

#[test]
fn test_ignores_acp_usage_cost() {
    // A peer may attach a monetary cost; our mirror stays token-based and
    // must decode the update while ignoring the cost object.
    let acp_val = acp::SessionUpdate::UsageUpdate(
        acp::UsageUpdate::new(1200, 200_000).cost(acp::Cost::new(0.03, "USD")),
    );
    let json = serde_json::to_string(&acp_val).expect("ACP serialize");
    assert!(json.contains(r#""cost""#), "ACP must emit cost: {json}");
    let ours: OurSessionUpdate = serde_json::from_str(&json).expect("decode ignores cost");
    match &ours {
        OurSessionUpdate::UsageUpdate(usage) => {
            assert_eq!(usage.used, 1200, "used");
            assert_eq!(usage.size, 200_000, "size");
        }
        _ => panic!("expected UsageUpdate, got {ours:?}"),
    }
}

#[test]
fn test_ours_acp_usage_update() {
    let ours = OurSessionUpdate::UsageUpdate(OurUsageUpdate {
        used: 1200,
        size: 200_000,
        meta: None,
    });
    let acp_val = ours_to_acp(&ours);
    match acp_val {
        acp::SessionUpdate::UsageUpdate(usage) => {
            assert_eq!(usage.used, 1200, "used");
            assert_eq!(usage.size, 200_000, "size");
            assert!(usage.cost.is_none(), "ours never emits cost");
        }
        _ => panic!("expected UsageUpdate"),
    }
}

// ---------------------------------------------------------------------------
// Unknown SessionUpdate variant is rejected (not silently dropped)
// ---------------------------------------------------------------------------

#[test]
fn test_unknown_session_update_fails() {
    // A variant our copy does not model must fail to decode — it is not
    // silently dropped to a default. This catches a peer emitting a variant
    // we have no representation for.
    let json = r#"{"sessionUpdate":"holographic_projection_update"}"#;
    let result: Result<OurSessionUpdate, _> = serde_json::from_str(json);
    assert!(
        result.is_err(),
        "unknown variant must not decode successfully"
    );
}
