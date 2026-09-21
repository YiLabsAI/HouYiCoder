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
