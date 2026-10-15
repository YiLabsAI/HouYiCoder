use super::*;

use houyicoder_protocol::frontend::ContentBlock;
use houyicoder_protocol::frontend::run::{
    ContentBlock as RunContentBlock, RunOutcome, RunResult, StopReason,
};
use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};
use houyicoder_protocol::llm::Usage;

use crate::composition::app;
use crate::transcript::TranscriptFrame;

/// A turn that produced neither reasoning nor a tool call is a plain
/// text reply; the rebuild must not surface a summary row for it, whose
/// affordance with no expandable content reads as broken.
#[test]
fn test_plain_reply_skips_thought() {
    let mut app = app();
    app.handle_run_completion(Ok(RunResult {
        outcome: RunOutcome::FinalOutput {
            content: vec![RunContentBlock::Text { text: "hi".into() }],
        },
        usage: Usage::default(),
        turns: 1,
        stop_reason: StopReason::EndTurn,
    }));
    let has_thought = app
        .sessions
        .current
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::ThoughtFor { .. }));
    assert!(!has_thought, "a plain reply must not push a ThoughtFor row");
}

/// A turn whose frames carry reasoning surfaces a summary row carrying that
/// reasoning, so the row is expandable. Deriving the row at the rebuild is
/// what lets a turn replayed from the log show the row the live turn showed.
#[test]
fn test_reasoning_turn_keeps_thought() {
    let mut app = app();
    app.sessions
        .current
        .transcript
        .push_frame(TranscriptFrame::Session(SessionUpdate::UserMessageChunk(
            ContentChunk::new(ContentBlock::Text { text: "go".into() }),
        )));
    app.sessions
        .current
        .transcript
        .push_frame(TranscriptFrame::Session(SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(ContentBlock::Text {
                text: "weighing the options".into(),
            }),
        )));
    app.handle_run_completion(Ok(RunResult {
        outcome: RunOutcome::FinalOutput {
            content: vec![ContentBlock::Text {
                text: "answer".into(),
            }],
        },
        usage: Usage::default(),
        turns: 1,
        stop_reason: StopReason::EndTurn,
    }));
    let has_thought = app.sessions.current.transcript.iter().any(|l| {
        matches!(
            l,
            TranscriptLine::ThoughtFor { reasoning: Some(r), .. }
                if r.contains("weighing the options")
        )
    });
    assert!(
        has_thought,
        "a turn with reasoning must surface its summary row"
    );
}
