//! Tests for this module.

use super::*;
use houyicoder_protocol::tool::is_user_shell_call_id;
use houyicoder_protocol::tool::user_shell_call_id;

/// An event id in its display spelling, the shape a user shell call id
/// carries.
const EVENT_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn tool_call(id: &str) -> OutputItem {
    OutputItem::ToolCall {
        id: id.to_string(),
        name: "bash".into(),
        input: serde_json::json!({ "command": "echo hi" }),
    }
}

/// A provider that picks the reserved shell shape would have its call read as
/// a command the user typed. The fold is the one step every model response
/// passes through, so the id is made distinct here, before the call reaches
/// the log or the transcript.
#[test]
fn test_response_renames_reserved() {
    let reserved = user_shell_call_id(EVENT_ID);
    let mut fold = StreamFold::default();
    fold.tool_calls.push(tool_call(&reserved));
    let response = fold.into_response("test".into());
    match &response.output[0] {
        OutputItem::ToolCall { id, .. } => {
            assert_eq!(id, &format!("model-{reserved}"));
            assert!(!is_user_shell_call_id(id));
        }
        other => panic!("expected a tool call, got {other:?}"),
    }
}

/// An ordinary provider id passes through verbatim, so the normalization
/// cannot disturb call ids the results answer.
#[test]
fn test_response_keeps_id() {
    let mut fold = StreamFold::default();
    fold.tool_calls.push(tool_call("call_abc"));
    let response = fold.into_response("test".into());
    match &response.output[0] {
        OutputItem::ToolCall { id, .. } => assert_eq!(id, "call_abc"),
        other => panic!("expected a tool call, got {other:?}"),
    }
}

/// Renaming a reserved-shaped id can land on a sibling the provider sent in
/// the same response. Both calls keep ids of their own, because every reader
/// that answers a call looks it up by id.
#[test]
fn test_response_keeps_ids_distinct() {
    let reserved = user_shell_call_id(EVENT_ID);
    let occupied = format!("model-{reserved}");
    let mut fold = StreamFold::default();
    fold.tool_calls.push(tool_call(&reserved));
    fold.tool_calls.push(tool_call(&occupied));
    let response = fold.into_response("test".into());
    let ids: Vec<&str> = response
        .output
        .iter()
        .filter_map(|item| match item {
            OutputItem::ToolCall { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "two calls cannot answer to one id");
}
