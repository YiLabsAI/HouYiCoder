use super::user_msg;
use crate::composition;
use crate::state::TranscriptLine;

/// A frame-by-frame replay must cap the projection too. Every other test here
/// loads the whole frame log and rebuilds once, which is the only shape where
/// the cap ever engaged: a replay that starts from an empty log has a window
/// start of 0 on its first rebuild, and folding that back into the prepend
/// floor latched the window open for the rest of the session, so each rebuild
/// re-projected the entire history. That is quadratic in the frame count --
/// a resumed session of 17k frames spent over a minute projecting before it
/// could paint, while a 700-frame one was fast enough to look fine.
#[test]
fn test_replay_caps_projection() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    for i in 0..600 {
        app.frames.push(user_msg(&format!("msg {i}")));
        app.rebuild_transcript();
    }
    assert!(
        app.transcript.len() < 600,
        "frame-by-frame replay should cap the projection, projected {} lines",
        app.transcript.len()
    );
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s.contains("msg 0"))),
        "earliest frame should not be projected (capped)"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s.contains("msg 599"))),
        "latest frame should be projected"
    );
}

#[test]
fn test_rebuild_caps_large_history() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    for i in 0..600 {
        app.frames.push(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    assert!(
        app.transcript.len() < 600,
        "should cap projection when frames > MAX_PROJECT_FRAMES, projected {} lines",
        app.transcript.len()
    );
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s.contains("msg 0"))),
        "earliest frame should not be projected (capped)"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s.contains("msg 599"))),
        "latest frame should be projected"
    );
}

#[test]
fn test_small_history_no_cap() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    for i in 0..10 {
        app.frames.push(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s.contains("msg 0"))),
        "earliest frame should be projected when under the cap"
    );
}

#[test]
fn test_progressive_prepend_loads_older() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    for i in 0..600 {
        app.frames.push(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    let lines_before = app.transcript.len();
    assert!(lines_before < 600, "capped after rebuild");
    // Simulate scroll to top: set offset to 0 + follow_tail false
    app.transcript_scroll.follow_tail = false;
    app.transcript_scroll.offset = 0;
    app.ensure_projected_above();
    assert!(
        app.transcript.len() > lines_before,
        "prepend should project older frames"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s.contains("msg 0"))),
        "earliest frame should now be visible after prepend"
    );
}

#[test]
fn test_progressive_prepend_noop_tail() {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    for i in 0..600 {
        app.frames.push(user_msg(&format!("msg {i}")));
    }
    app.rebuild_transcript();
    let lines_before = app.transcript.len();
    // follow_tail = true: user is at the bottom, no prepend
    app.ensure_projected_above();
    assert_eq!(
        app.transcript.len(),
        lines_before,
        "no prepend when following tail"
    );
}

/// BUG 1 regression: prepend must update sealed_transcript_len so the next
/// incremental rebuild treats prepended lines as sealed prefix, not tail
/// to overwrite.
#[test]
fn test_prepend_incremental_keeps_prepended() {
    use crate::transcript::TranscriptFrame;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    // 600 frames: first 100 are "old", then a user message (turn boundary),
    // then 500 more.
    for i in 0..100 {
        app.frames.push(user_msg(&format!("old {i}")));
    }
    app.frames
        .push(TranscriptFrame::Session(SessionUpdate::UserMessageChunk(
            ContentChunk::new(ContentBlock::Text {
                text: "turn boundary".into(),
            }),
        )));
    for i in 0..500 {
        app.frames.push(user_msg(&format!("recent {i}")));
    }
    app.rebuild_transcript();
    assert!(app.projected_from_frame.get() > 0, "capped");
    // Scroll to top + prepend
    app.transcript_scroll.follow_tail = false;
    app.transcript_scroll.offset = 0;
    app.ensure_projected_above();
    let has_old = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::User(s) if s.contains("old 1")));
    assert!(has_old, "old content prepended");
    // Simulate a new non-turn-boundary frame arriving (incremental rebuild).
    // Using AgentMessageChunk (not UserMessageChunk) so turn_start stays the
    // same and the incremental path runs (not need_full).
    app.frames
        .push(TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text {
                text: "new after prepend".into(),
            }),
        )));
    app.rebuild_transcript();
    // The old content must still be there (not overwritten by incremental)
    let still_has_old = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::User(s) if s.contains("old 1")));
    assert!(
        still_has_old,
        "prepended content must survive incremental rebuild (sealed_transcript_len updated)"
    );
}
