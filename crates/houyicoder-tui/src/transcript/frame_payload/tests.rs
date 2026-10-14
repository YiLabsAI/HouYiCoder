use super::*;
use houyicoder_protocol::frontend::session_update::ToolCallUpdateFields;

fn sized(frame: TranscriptFrame) -> usize {
    SequencedFrame::from(frame).estimated_bytes()
}

/// The two payloads a frame can carry without bound, an image's encoded
/// data and a tool's output, must reach the resident total; a budget that
/// counts them as nothing never evicts the frames that exhaust memory.
#[test]
fn test_sizes_image_and_output() {
    let image = TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
        ContentBlock::Image {
            data: "A".repeat(5000),
            mime_type: "image/png".into(),
        },
    )));
    let result = TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "call-1",
        ToolCallUpdateFields::new().raw_output(serde_json::json!({ "stdout": "B".repeat(5000) })),
    )));
    let text = TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
        ContentBlock::Text {
            text: "C".repeat(20),
        },
    )));

    assert!(sized(image) >= 5000, "an image's data is resident");
    assert!(sized(result) >= 5000, "a tool result's output is resident");
    assert!(sized(text) < 500, "a short text frame rides its overhead");
}

/// A nested value's cost must include its inner strings, not just its
/// outer keys.
#[test]
fn test_sizes_nested_json() {
    let shallow = json_bytes(&serde_json::json!({ "a": "x" }));
    let nested = json_bytes(&serde_json::json!({ "a": { "b": { "c": "x".repeat(400) } } }));
    assert!(nested >= 400, "the innermost string is counted");
    assert!(shallow < 32, "a one-field value stays small");
}
