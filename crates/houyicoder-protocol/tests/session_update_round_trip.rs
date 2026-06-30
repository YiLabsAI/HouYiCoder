//! Self round-trip: each SessionUpdate variant serializes to JSON and
//! deserializes back to the same typed value, and the reserialized JSON is
//! byte-identical to the original. Plus exact wire-JSON shape assertions for
//! one AgentMessageChunk and one ToolCall so the documented wire contract is
//! gated. This file depends on serde only — the protocol layer is serde-only
//! and must not import the ACP crate.

use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use serde_json::Value;

/// Assert a value survives a serialize-deserialize-reserialize cycle with
/// stable JSON: the deserialized value re-emits the same bytes. This is the
/// round-trip stability contract — a peer re-encoding a message it received
/// produces the same bytes.
fn round_trips(u: &SessionUpdate) {
    let json = serde_json::to_string(u).expect("serialize");
    let back: SessionUpdate = serde_json::from_str(&json).expect("deserialize");
    let back_json = serde_json::to_string(&back).expect("reserialize");
    assert_eq!(json, back_json, "round-trip not stable: {json}");
}

// ---------------------------------------------------------------------------
// Each variant round-trips
// ---------------------------------------------------------------------------

#[test]
fn test_user_message_chunk_trips() {
    let u = SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text {
        text: "user input".into(),
    }));
    round_trips(&u);
}

#[test]
fn test_agent_message_chunk_trips() {
    let u = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text {
        text: "agent reply".into(),
    }));
    round_trips(&u);
}

#[test]
fn test_agent_thought_chunk_trips() {
    let u = SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text {
        text: "agent reasoning".into(),
    }));
    round_trips(&u);
}

#[test]
fn test_tool_call_round_trips() {
    let u = SessionUpdate::ToolCall(
        ToolCall::new("toolu_42", "read_file")
            .raw_input(Value::String(r#"{"path":"src/lib.rs"}"#.into()))
            .status(ToolCallStatus::InProgress),
    );
    round_trips(&u);
}

#[test]
fn test_tool_update_round_trips() {
    let u = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "toolu_42",
        ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(Value::String("done".into())),
    ));
    round_trips(&u);
}

// ---------------------------------------------------------------------------
// Exact wire JSON shapes
// ---------------------------------------------------------------------------

#[test]
fn test_agent_message_chunk_wire() {
    // Internally tagged by sessionUpdate; the content chunk nests a
    // type-tagged content block. The wire shape is the documented ACP
    // session/update shape.
    let u = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text {
        text: "hi".into(),
    }));
    let json = serde_json::to_string(&u).expect("serialize");
    assert_eq!(
        json,
        r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}"#
    );
}

#[test]
fn test_tool_call_exact_wire() {
    // camelCase fields; tool_call_id is a transparent string. The kind
    // default (Other) and status default (Pending) are skipped so the wire
    // shape matches an emission that omits them.
    let u = SessionUpdate::ToolCall(
        ToolCall::new("toolu_1", "bash")
            .raw_input(Value::String("ls".into()))
            .status(ToolCallStatus::InProgress),
    );
    let json = serde_json::to_string(&u).expect("serialize");
    assert_eq!(
        json,
        r#"{"sessionUpdate":"tool_call","toolCallId":"toolu_1","title":"bash","status":"in_progress","rawInput":"ls"}"#
    );
}

#[test]
fn test_tool_update_exact_wire() {
    // The update fields are flattened in: the wire shape is tool_call_id
    // plus the updated fields inlined, not nested under a fields key.
    let u = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "toolu_1",
        ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(Value::String("ok".into())),
    ));
    let json = serde_json::to_string(&u).expect("serialize");
    assert_eq!(
        json,
        r#"{"sessionUpdate":"tool_call_update","toolCallId":"toolu_1","status":"completed","rawOutput":"ok"}"#
    );
}

// ---------------------------------------------------------------------------
// Default-valued fields are skipped on serialize
// ---------------------------------------------------------------------------

#[test]
fn test_tool_call_fields_skipped() {
    // A ToolCall with only tool_call_id and title emits just those two
    // fields plus the sessionUpdate tag — kind (Other) and status (Pending)
    // defaults are skipped, and empty content/locations are omitted.
    let u = SessionUpdate::ToolCall(ToolCall::new("toolu_1", "grep"));
    let json = serde_json::to_string(&u).expect("serialize");
    assert_eq!(
        json,
        r#"{"sessionUpdate":"tool_call","toolCallId":"toolu_1","title":"grep"}"#
    );
}

#[test]
fn test_tool_call_partial_fields() {
    // An update carrying only status emits just tool_call_id and status —
    // the unset raw_output and title are not present.
    let u = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "toolu_2",
        ToolCallUpdateFields::new().status(ToolCallStatus::Failed),
    ));
    let json = serde_json::to_string(&u).expect("serialize");
    assert_eq!(
        json,
        r#"{"sessionUpdate":"tool_call_update","toolCallId":"toolu_2","status":"failed"}"#
    );
}

// ---------------------------------------------------------------------------
// Unknown fields are ignored on decode
// ---------------------------------------------------------------------------

#[test]
fn test_ignores_unknown_fields_decode() {
    // A peer emission carrying fields the server does not emit still parses:
    // serde ignores unknown fields, so decode parity holds for the subset we
    // model.
    let json = r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi","annotations":{"priority":"high"},"_meta":{"x":1}}}"#;
    let back: SessionUpdate = serde_json::from_str(json).expect("decode peer shape");
    match &back {
        SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
            ContentBlock::Text { text } => assert_eq!(text, "hi"),
            _ => panic!("expected text content block"),
        },
        _ => panic!("expected AgentMessageChunk, got {back:?}"),
    }
}
