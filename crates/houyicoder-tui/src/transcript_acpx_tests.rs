//! Acpx notification surfacing in the transcript.

use houyicoder_protocol::acpx::{AcpxMethod, AcpxNotification};
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};

use crate::records::TranscriptLine;
use crate::transcript::{TranscriptFrame, transcript_from_frames};

fn user_msg(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}

/// Compaction boundary + summary ride the acpx stream; both become
/// System lines at their ordered positions. The meta-user nudge and
/// permission-decision audit do not surface.
#[test]
fn test_acpx_compaction_summary_surface() {
    let frames = vec![
        user_msg("hi"),
        TranscriptFrame::Acpx(AcpxNotification::new(
            AcpxMethod::ContextCompactionBoundary,
            serde_json::json!({ "checkpoint": "01J00000000000000000000000" }),
        )),
        TranscriptFrame::Acpx(AcpxNotification::new(
            AcpxMethod::ContextSummary,
            serde_json::json!({ "text": "prior turn condensed" }),
        )),
        TranscriptFrame::Acpx(AcpxNotification::new(
            AcpxMethod::ContextMetaUser,
            serde_json::json!({ "text": "nudge" }),
        )),
    ];
    let lines = transcript_from_frames(&frames);
    // User + compaction + summary; meta-user dropped.
    assert_eq!(lines.len(), 3);
    assert!(matches!(lines[0], TranscriptLine::User(_)));
    assert!(matches!(
        lines[1],
        TranscriptLine::System(ref s) if s == "compaction checkpoint"
    ));
    assert!(matches!(
        lines[2],
        TranscriptLine::System(ref s) if s == "summary: prior turn condensed"
    ));
}
