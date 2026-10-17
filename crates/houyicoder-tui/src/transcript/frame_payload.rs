//! Reading what a frame carries: the text of a content chunk, and the resident
//! byte cost of a frame. A long session's memory is spent on the frame log's
//! payload text and JSON, so the resident window's budget is sized from the
//! frames' own payloads rather than from a frame count.

use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::ContentChunk;

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

/// Fixed cost of one frame: the enum tag, the sequence number, and the vector
/// slot it occupies.
const FRAME_OVERHEAD: usize = 64;

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
/// a long session's memory is spent on. A protocol payload is sized by the
/// crate that defines it, so a holder reads one figure instead of keeping a
/// walk of its own. Variants with no large payload return zero and ride the
/// per-frame overhead.
fn frame_payload_bytes(frame: &TranscriptFrame) -> usize {
    match frame {
        TranscriptFrame::Session(update) => update.estimated_bytes(),
        TranscriptFrame::Acpx(note) => note.estimated_bytes(),
        TranscriptFrame::Frontend(FrontendRow::System(text))
        | TranscriptFrame::Frontend(FrontendRow::Echo(text)) => text.len(),
        // The context grid is a small bounded structure of counters and
        // labels; a flat estimate covers it without walking every field.
        TranscriptFrame::Frontend(FrontendRow::Context(_)) => CONTEXT_FRAME_BYTES,
        TranscriptFrame::Frontend(_) => 0,
    }
}

#[cfg(test)]
mod tests;
