//! Model-to-runtime mapping: the provider and window a config entry resolves to.

use super::*;
use houyicoder_context::SessionId;

fn sid() -> SessionId {
    SessionId::from_display_string("00000000-0000-0000-0000-000000000000").unwrap()
}

fn rec_user(content: &str, is_meta: bool) -> Value {
    serde_json::json!({
        "type": "user",
        "isMeta": is_meta,
        "message": {"role": "user", "content": content},
        "timestamp": "2026-08-02T15:38:23.123Z",
    })
}

fn rec_assistant(blocks: Vec<Value>) -> Value {
    serde_json::json!({
        "type": "assistant",
        "message": {"role": "assistant", "model": "glm-5.2", "content": blocks},
        "timestamp": "2026-08-02T15:38:24.000Z",
    })
}

fn kinds(events: &[SessionLogEntry]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match &e.event {
            SessionEvent::UserInput { .. } => "UserInput",
            SessionEvent::MetaUser { .. } => "MetaUser",
            SessionEvent::AssistantMessage { .. } => "AssistantMessage",
            SessionEvent::Reasoning { .. } => "Reasoning",
            SessionEvent::ToolCall { .. } => "ToolCall",
            SessionEvent::ToolResult { .. } => "ToolResult",
            _ => "other",
        })
        .collect()
}

#[test]
fn test_user_string_maps_userinput() {
    let mut model = None;
    let mut cwd = None;
    let ev = map_record(&rec_user("hello", false), sid(), 0, &mut model, &mut cwd);
    assert_eq!(kinds(&ev), vec!["UserInput"]);
    match &ev[0].event {
        SessionEvent::UserInput { text } => assert_eq!(text, "hello"),
        _ => unreachable!(),
    }
}

#[test]
fn test_user_meta_maps_metauser() {
    let mut model = None;
    let mut cwd = None;
    let ev = map_record(&rec_user("nudge", true), sid(), 0, &mut model, &mut cwd);
    assert_eq!(kinds(&ev), vec!["MetaUser"]);
}

#[test]
fn test_assistant_blocks_emit_all() {
    let blocks = vec![
        serde_json::json!({"type": "thinking", "thinking": "let me think"}),
        serde_json::json!({"type": "tool_use", "id": "tu_1", "name": "bash", "input": {"cmd": "ls"}}),
        serde_json::json!({"type": "text", "text": "done"}),
    ];
    let mut model = None;
    let mut cwd = None;
    let ev = map_record(&rec_assistant(blocks), sid(), 0, &mut model, &mut cwd);
    assert_eq!(
        kinds(&ev),
        vec!["Reasoning", "ToolCall", "AssistantMessage"]
    );
    // The thinking is folded into the AssistantMessage.
    match ev.last().unwrap().event {
        SessionEvent::AssistantMessage {
            ref text,
            ref thinking,
        } => {
            assert_eq!(text, "done");
            assert_eq!(thinking.as_deref(), Some("let me think"));
        }
        _ => unreachable!(),
    }
    // Model + the tool call fields round-trip.
    assert_eq!(model.as_deref(), Some("glm-5.2"));
    match ev[1].event {
        SessionEvent::ToolCall {
            ref call_id,
            ref tool,
            ..
        } => {
            assert_eq!(call_id, "tu_1");
            assert_eq!(tool, "bash");
        }
        _ => unreachable!(),
    }
}

#[test]
fn test_user_tool_result_maps() {
    let rec = serde_json::json!({
        "type": "user",
        "isMeta": false,
        "message": {
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "tu_1", "content": "file listing"}],
        },
        "timestamp": "2026-08-02T15:38:25.000Z",
    });
    let mut model = None;
    let mut cwd = None;
    let ev = map_record(&rec, sid(), 0, &mut model, &mut cwd);
    assert_eq!(kinds(&ev), vec!["ToolResult"]);
    match ev[0].event {
        SessionEvent::ToolResult { ref call_id, .. } => assert_eq!(call_id, "tu_1"),
        _ => unreachable!(),
    }
}

#[test]
fn test_skipped_types_yield_nothing() {
    let mut model = None;
    let mut cwd = None;
    for ty in [
        "mode",
        "permission-mode",
        "system",
        "attachment",
        "file-history-snapshot",
    ] {
        let rec =
            serde_json::json!({"type": ty, "sessionId": "00000000-0000-0000-0000-000000000000"});
        let ev = map_record(&rec, sid(), 0, &mut model, &mut cwd);
        assert!(ev.is_empty(), "{ty} should map to no events");
    }
}

#[test]
fn test_timestamp_parses_to_ms() {
    assert!(parse_ts_ms("2026-08-02T15:38:23.123Z") > 0);
    assert_eq!(parse_ts_ms("not a date"), 0);
}
