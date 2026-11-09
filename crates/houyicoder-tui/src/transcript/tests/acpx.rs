//! Acpx notification surfacing in the transcript.

use houyicoder_protocol::acpx::{AcpxMethod, AcpxNotification};
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};

use crate::records::TranscriptLine;
use crate::transcript::frame::TranscriptFrame;
use crate::transcript::transcript_from_frames;

fn user_msg(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}

/// A compaction boundary and the summary that follows it are one event, so
/// they draw one row: the event's name with the summary folded into it as its
/// body. As two plain rows the checkpoint said nothing a reader needed and the
/// summary pasted the model's whole digest across the screen. The meta-user
/// nudge and the permission-decision audit still do not surface.
#[test]
fn test_compaction_one_row() {
    let frames = vec![
        user_msg("hi"),
        TranscriptFrame::Acpx(AcpxNotification::new(
            AcpxMethod::ContextCompactionBoundary,
            serde_json::json!({
                "checkpoint": "01J00000000000000000000000",
                "preTokens": 90000,
                "postTokens": 12000,
            }),
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
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    // User + one compaction row; the summary folded in, the nudge dropped.
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(matches!(lines[0], TranscriptLine::User(_)));
    let TranscriptLine::Compaction { stats, summary, .. } = &lines[1] else {
        panic!("one compaction row: {:?}", lines[1]);
    };
    assert!(
        stats.contains("90k -> 12k tokens"),
        "the statistics the boundary reported are kept: {stats}"
    );
    assert_eq!(
        summary, "prior turn condensed",
        "the summary is the row's body, not a row of its own"
    );
}

/// ATTACK by counterexample: boundary, boundary, summary, summary. Two
/// compactions in one window is rare but the wire permits it, and the join is
/// by append order alone. The second summary must not land on the same row as
/// the first, leaving one digest lost and one overwritten.
#[test]
fn test_paired_digests() {
    let frames = vec![
        user_msg("hi"),
        TranscriptFrame::Acpx(AcpxNotification::new(
            AcpxMethod::ContextCompactionBoundary,
            serde_json::json!({ "checkpoint": "cp-one" }),
        )),
        TranscriptFrame::Acpx(AcpxNotification::new(
            AcpxMethod::ContextCompactionBoundary,
            serde_json::json!({ "checkpoint": "cp-two" }),
        )),
        TranscriptFrame::Acpx(AcpxNotification::new(
            AcpxMethod::ContextSummary,
            serde_json::json!({ "text": "first digest" }),
        )),
        TranscriptFrame::Acpx(AcpxNotification::new(
            AcpxMethod::ContextSummary,
            serde_json::json!({ "text": "second digest" }),
        )),
    ];
    let lines = transcript_from_frames(&frames, 0..frames.len(), false);
    let compactions: Vec<&TranscriptLine> = lines
        .iter()
        .filter(|l| matches!(l, TranscriptLine::Compaction { .. }))
        .collect();
    assert_eq!(compactions.len(), 2, "one row per boundary: {lines:?}");
    let digests: Vec<String> = compactions
        .iter()
        .filter_map(|l| match l {
            TranscriptLine::Compaction { summary, .. } if !summary.is_empty() => {
                Some(summary.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        digests.len(),
        2,
        "both digests are kept, not one overwriting the other: {lines:?}"
    );
}
