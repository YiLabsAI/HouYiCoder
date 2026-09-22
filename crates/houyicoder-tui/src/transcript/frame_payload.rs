//! Reading what a frame carries: the text of a content chunk, and the resident
//! byte cost of a frame. A long session's memory is spent on the frame log's
//! payload text and JSON, so the resident window's budget is sized from the
//! frames' own payloads rather than from a frame count. Every payload that can
//! grow without bound is walked: message text, image data, tool input, tool
//! output, and the audit stream's parameters.

use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallContent, ToolCallUpdate,
};
use serde_json::Value;

use super::{FrontendRow, SequencedFrame, TranscriptFrame};

/// The text carried by a content chunk, when the chunk wraps a text block.
/// Non-text blocks (Image) have no flat text; an empty string degenerates the
/// line away so a multimodal chunk does not surface as an empty row.
pub fn chunk_text(chunk: &ContentChunk) -> &str {
    match &chunk.content {
        ContentBlock::Text { text } => text.as_str(),
        _ => "",
    }
}

/// Flat resident cost attributed to one context-grid frame, whose counters and
/// labels are a small bounded structure.
const CONTEXT_FRAME_BYTES: usize = 1024;

/// Fixed cost of one frame: the enum tag, the seq, the vec slot, and the
/// payload's own struct headers.
const FRAME_OVERHEAD: usize = 64;

/// Cost of a JSON value that carries no string of its own, covering the digits
/// or keyword plus the separator that surrounds it.
const JSON_SCALAR_BYTES: usize = 8;

impl SequencedFrame {
    /// A rough byte cost of holding this frame resident. The estimate sums the
    /// salient string and JSON payloads over a fixed per-frame overhead. It
    /// sizes the resident budget, not an exact allocation count, so a variant
    /// whose payload is not walked still contributes its overhead.
    pub fn estimated_bytes(&self) -> usize {
        FRAME_OVERHEAD + frame_payload_bytes(&self.frame)
    }
}

/// The payload bytes of one frame: the text and JSON it carries, which is what
/// a long session's memory is spent on. Variants with no large payload return
/// zero and ride the per-frame overhead.
fn frame_payload_bytes(frame: &TranscriptFrame) -> usize {
    match frame {
        TranscriptFrame::Session(
            SessionUpdate::UserMessageChunk(chunk)
            | SessionUpdate::AgentMessageChunk(chunk)
            | SessionUpdate::AgentThoughtChunk(chunk),
        ) => content_block_bytes(&chunk.content),
        TranscriptFrame::Session(SessionUpdate::ToolCall(call)) => tool_call_bytes(call),
        TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(update)) => {
            tool_update_bytes(update)
        }
        TranscriptFrame::Acpx(note) => json_bytes(&note.params),
        TranscriptFrame::Frontend(FrontendRow::System(text))
        | TranscriptFrame::Frontend(FrontendRow::Echo(text)) => text.len(),
        // The context grid is a small bounded structure of counters and
        // labels; a flat estimate covers it without walking every field.
        TranscriptFrame::Frontend(FrontendRow::Context(_)) => CONTEXT_FRAME_BYTES,
        TranscriptFrame::Frontend(_) => 0,
        TranscriptFrame::Session(_) => 0,
    }
}

/// The resident bytes of one content block. An image carries its encoded data
/// inline, so it can outweigh every text frame in the window.
fn content_block_bytes(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => text.len(),
        ContentBlock::Image { data, mime_type } => data.len() + mime_type.len(),
        // A block kind outside the two above carries no payload this budget
        // knows to size.
        _ => 0,
    }
}

/// The resident bytes of a tool call. The raw input and output dominate, since
/// the arguments an edit or a command carries are far larger than its title.
fn tool_call_bytes(call: &ToolCall) -> usize {
    call.tool_call_id.0.len()
        + call.title.len()
        + call.content.iter().map(tool_content_bytes).sum::<usize>()
        + call.locations.iter().map(|at| at.path.len()).sum::<usize>()
        + call.raw_input.as_ref().map_or(0, json_bytes)
        + call.raw_output.as_ref().map_or(0, json_bytes)
}

/// The resident bytes of one rendered tool result, which wraps a content block
/// the same way a message chunk does.
fn tool_content_bytes(content: &ToolCallContent) -> usize {
    match content {
        ToolCallContent::Content { content } => content_block_bytes(content),
        _ => 0,
    }
}

/// The resident bytes of a tool call update, which carries the result a call
/// produced: the output a command printed, or the file it read.
fn tool_update_bytes(update: &ToolCallUpdate) -> usize {
    update.tool_call_id.0.len()
        + update.fields.title.as_ref().map_or(0, String::len)
        + update.fields.raw_input.as_ref().map_or(0, json_bytes)
        + update.fields.raw_output.as_ref().map_or(0, json_bytes)
}

/// The resident byte cost of a JSON payload, sized by walking it. Serializing
/// would allocate a throwaway string on every appended frame and measure the
/// wire form; the walk measures what the frame holds, which is what the budget
/// bounds.
fn json_bytes(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => JSON_SCALAR_BYTES,
        Value::String(text) => text.len() + 2,
        Value::Array(items) => 2 + items.iter().map(json_bytes).sum::<usize>(),
        Value::Object(entries) => {
            2 + entries
                .iter()
                .map(|(key, value)| key.len() + 3 + json_bytes(value))
                .sum::<usize>()
        }
    }
}

#[cfg(test)]
mod tests {
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
            ToolCallUpdateFields::new()
                .raw_output(serde_json::json!({ "stdout": "B".repeat(5000) })),
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
}
