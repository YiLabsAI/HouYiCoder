//! Unit tests for stream_decoder covering SSE delta parsing and closing states.

use super::*;
use serde_json::json;

#[test]
fn test_chunk_empty_and_usage() {
    let mut decoder = StreamDecoder::default();
    let mut out = Vec::new();

    // Empty chunk without choices
    let chunk = json!({"choices": []});
    assert_eq!(
        decoder.handle_chunk(&chunk, &mut out),
        ChunkAction::Continue
    );
    assert!(out.is_empty());

    // Chunk carrying usage
    let chunk = json!({
        "choices": [],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "total_tokens": 150
        }
    });
    assert_eq!(
        decoder.handle_chunk(&chunk, &mut out),
        ChunkAction::Continue
    );
    assert_eq!(decoder.final_usage.as_ref().unwrap().input_tokens, 100);
}

#[test]
fn test_text_and_reasoning_flow() {
    let mut decoder = StreamDecoder::default();
    let mut out = Vec::new();

    // Reasoning delta
    let chunk = json!({
        "choices": [{
            "index": 0,
            "delta": {"reasoning_content": "thinking..."}
        }]
    });
    assert_eq!(
        decoder.handle_chunk(&chunk, &mut out),
        ChunkAction::Continue
    );
    assert_eq!(out.len(), 2);
    assert!(matches!(out[0], LlmEvent::ReasoningStart { .. }));
    assert!(matches!(out[1], LlmEvent::ReasoningDelta { .. }));

    // Content delta transitions reasoning -> text
    out.clear();
    let chunk = json!({
        "choices": [{
            "index": 0,
            "delta": {"content": "hello"}
        }]
    });
    assert_eq!(
        decoder.handle_chunk(&chunk, &mut out),
        ChunkAction::Continue
    );
    assert_eq!(out.len(), 3);
    assert!(matches!(out[0], LlmEvent::ReasoningEnd { .. }));
    assert!(matches!(out[1], LlmEvent::TextStart { .. }));
    assert!(matches!(out[2], LlmEvent::TextDelta { .. }));
}

#[test]
fn test_tool_calls_and_finish() {
    let mut decoder = StreamDecoder::default();
    let mut out = Vec::new();

    // Tool call delta
    let chunk = json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "tc1",
                    "function": {"name": "read_file", "arguments": "{\"path\": "}
                }]
            }
        }]
    });
    assert_eq!(
        decoder.handle_chunk(&chunk, &mut out),
        ChunkAction::Continue
    );

    // Second fragment + finish reason
    let chunk = json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "\"foo.rs\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    assert_eq!(
        decoder.handle_chunk(&chunk, &mut out),
        ChunkAction::Continue
    );
    assert!(decoder.finish_reason.is_some());
    assert!(out.iter().any(|e| matches!(e, LlmEvent::ToolCall { .. })));

    // Late trailing usage breaks the stream
    let chunk = json!({
        "choices": [],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15
        }
    });
    assert_eq!(
        decoder.handle_chunk(&chunk, &mut out),
        ChunkAction::BreakStream
    );

    let finish_events = decoder.finish_events();
    assert_eq!(finish_events.len(), 2);
    assert!(matches!(finish_events[0], LlmEvent::StepFinish { .. }));
    assert!(matches!(finish_events[1], LlmEvent::Finish { .. }));
}

/// A tool-call reply publishes the argument stream as it arrives: the fragment
/// that names the call opens it, later fragments extend it, and the stream
/// closes before the call itself so a reader can see both ends.
#[test]
fn test_tool_delta_opens_input() {
    let mut decoder = StreamDecoder::default();
    let mut out = Vec::new();

    let chunk = json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "tc1",
                    "function": {"name": "read_file", "arguments": "{\"path\": "}
                }]
            }
        }]
    });
    decoder.handle_chunk(&chunk, &mut out);
    assert_eq!(
        out.len(),
        2,
        "the opening fragment names the call and extends it: {out:?}"
    );
    assert!(matches!(
        &out[0],
        LlmEvent::ToolInputStart { id, name } if id == "tc1" && name == "read_file"
    ));
    assert!(matches!(
        &out[1],
        LlmEvent::ToolInputDelta { text, .. } if text == "{\"path\": "
    ));

    out.clear();
    let chunk = json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "\"a.rs\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    decoder.handle_chunk(&chunk, &mut out);
    let kinds: Vec<&str> = out
        .iter()
        .map(|e| match e {
            LlmEvent::ToolInputDelta { .. } => "delta",
            LlmEvent::ToolInputEnd { .. } => "end",
            LlmEvent::ToolCall { .. } => "call",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["delta", "end", "call"],
        "the arguments end before the call they built: {out:?}"
    );
    assert!(matches!(
        &out[2],
        LlmEvent::ToolCall { input, .. } if input["path"] == "a.rs"
    ));
}

/// A fragment that carries arguments before the call is named publishes
/// nothing of its own: the events name the call they belong to, and a name
/// that has not arrived yet cannot open it. The fragment that names the call
/// opens the block with every argument carried so far, so a reader
/// reconstructing the input from the deltas sees the whole string.
#[test]
fn test_unnamed_fragment_defers_start() {
    let mut decoder = StreamDecoder::default();
    let mut out = Vec::new();

    let chunk = json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"path\":"}}]}
        }]
    });
    decoder.handle_chunk(&chunk, &mut out);
    assert!(out.is_empty(), "no call to name yet: {out:?}");

    out.clear();
    let chunk = json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "tc1",
                    "function": {"name": "read_file", "arguments": "\"a.rs\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    decoder.handle_chunk(&chunk, &mut out);
    assert!(matches!(
        &out[0],
        LlmEvent::ToolInputStart { name, .. } if name == "read_file"
    ));
    assert!(matches!(
        &out[1],
        LlmEvent::ToolInputDelta { text, .. } if text == "{\"path\":\"a.rs\"}"
    ));
    let printed: String = out
        .iter()
        .filter_map(|e| match e {
            LlmEvent::ToolInputDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        printed, "{\"path\":\"a.rs\"}",
        "the deltas a reader saw spell out the call's input: {out:?}"
    );
    assert!(
        matches!(
            out.last(),
            Some(LlmEvent::ToolCall { input, .. }) if input["path"] == "a.rs"
        ),
        "both fragments reach the call: {out:?}"
    );
}

/// A stream that ends without a finish reason still closes the argument stream
/// it opened, so a call cut mid-arguments is measured rather than left open.
#[test]
fn test_abnormal_close_ends_input() {
    let mut decoder = StreamDecoder::default();
    let mut out = Vec::new();
    let chunk = json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "tc1",
                    "function": {"name": "read_file", "arguments": "{\"path\": "}
                }]
            }
        }]
    });
    decoder.handle_chunk(&chunk, &mut out);
    out.clear();

    let events = decoder.finish_events();
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            LlmEvent::ToolInputEnd { .. } => "end",
            LlmEvent::ToolCall { .. } => "call",
            LlmEvent::StepFinish { .. } => "step",
            LlmEvent::Finish { .. } => "finish",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["end", "call", "step", "finish"],
        "the cut call's arguments end before it is dispatched: {events:?}"
    );
}

#[test]
fn test_abnormal_close_length() {
    let mut decoder = StreamDecoder::default();
    let mut out = Vec::new();
    let chunk = json!({
        "choices": [{
            "index": 0,
            "delta": {"content": "incomplete"}
        }]
    });
    decoder.handle_chunk(&chunk, &mut out);

    let finish_events = decoder.finish_events();
    assert!(
        finish_events
            .iter()
            .any(|e| matches!(e, LlmEvent::Finish { reason, .. } if reason == "length"))
    );
}
