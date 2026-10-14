//! Tests for this module.

use super::*;
use serde_json::Value;

#[test]
fn test_agent_message_round_trips() {
    // Internally tagged by sessionUpdate; the content chunk nests a
    // type-tagged content block. The wire shape is the ACP session/update
    // shape the cross-decode fixture gates.
    let u = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text {
        text: "hi".into(),
    }));
    let json = serde_json::to_string(&u).expect("serialize");
    assert_eq!(
        json,
        r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}"#
    );
    let back: SessionUpdate = serde_json::from_str(&json).expect("deserialize");
    assert!(matches!(back, SessionUpdate::AgentMessageChunk(_)));
}

#[test]
fn test_tool_call_round_trips() {
    // camelCase fields; tool_call_id is a transparent string. The kind
    // and status defaults are skipped when they hold the default so
    // the wire shape matches an emission that omits them.
    let tc = SessionUpdate::ToolCall(
        ToolCall::new("toolu_1", "bash")
            .raw_input(Value::String("ls".into()))
            .status(ToolCallStatus::InProgress),
    );
    let json = serde_json::to_string(&tc).expect("serialize");
    assert!(
        json.contains(r#""toolCallId":"toolu_1""#),
        "camelCase: {json}"
    );
    assert!(json.contains(r#""title":"bash""#), "title: {json}");
    assert!(json.contains(r#""status":"in_progress""#), "status: {json}");
    assert!(json.contains(r#""rawInput":"ls""#), "rawInput: {json}");
    assert!(!json.contains("kind"), "default kind omitted: {json}");
}

#[test]
fn test_tool_call_update_flattens() {
    // The update fields are flattened in: the wire shape is tool_call_id
    // plus the updated fields inlined, not nested under a fields key.
    let upd = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "toolu_1",
        ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(Value::String("ok".into())),
    ));
    let json = serde_json::to_string(&upd).expect("serialize");
    assert!(
        !json.contains(r#""fields""#),
        "fields flattened, not nested: {json}"
    );
    assert!(json.contains(r#""toolCallId":"toolu_1""#), "{json}");
    assert!(json.contains(r#""status":"completed""#), "{json}");
    assert!(json.contains(r#""rawOutput":"ok""#), "{json}");
}

#[test]
fn test_ignores_unknown_fields() {
    // A peer emission carrying _meta / annotations the server does not
    // emit still parses: serde ignores unknown fields, so decode parity
    // holds for the subset we model.
    let json = r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi","annotations":{"priority":"high"},"_meta":{"x":1}}}"#;
    let back: SessionUpdate = serde_json::from_str(json).expect("decode peer shape");
    assert!(matches!(back, SessionUpdate::AgentMessageChunk(_)));
}

#[test]
fn test_chunks_round_trip() {
    for (name, upd) in [
        (
            "user_message_chunk",
            SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text {
                text: "u".into(),
            })),
        ),
        (
            "agent_thought_chunk",
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text {
                text: "t".into(),
            })),
        ),
    ] {
        let json = serde_json::to_string(&upd).expect("serialize");
        assert!(json.contains(&format!("\"{name}\"")), "{json}");
        let back: SessionUpdate = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(json, serde_json::to_string(&back).unwrap());
    }
}
