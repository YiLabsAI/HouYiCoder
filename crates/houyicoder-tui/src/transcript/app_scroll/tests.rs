use super::*;
use crate::composition;
use crate::transcript::SequencedFrame;
use houyicoder_protocol::envelope::{DurableEventId, EventSeq};
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::ContentChunk;

fn stamped_frame(id: &str, text: &str) -> SequencedFrame {
    SequencedFrame {
        seq: Some(EventSeq(1)),
        event_id: Some(DurableEventId(id.to_string())),
        frame: TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text { text: text.into() },
        ))),
    }
}

/// A tail frame carrying a durable identity anchors the cursor on that
/// entry, so the baseline survives a front drain that a raw frame index
/// would not, and a re-read the index would misname.
#[test]
fn test_tail_cursor_durable_anchored() {
    let mut app = composition::app();
    app.sessions
        .current
        .transcript
        .push_frame(stamped_frame("e1", "first"));
    let expected = RowId::Durable {
        event: DurableEventId("e1".to_string()),
        slot: 0,
    };
    assert_eq!(app.event_cursor_at_tail(), EventCursor::Durable(expected));
}

/// A tail frame with no durable identity anchors Local on the absolute
/// frame count, which a front drain does not shift.
#[test]
fn test_tail_cursor_local_count() {
    let mut app = composition::app();
    app.sessions
        .current
        .transcript
        .push_frame(TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text { text: "hi".into() }),
        )));
    assert!(matches!(
        app.event_cursor_at_tail(),
        EventCursor::Local(n) if n == 1
    ));
}

/// An empty transcript anchors Local(0) so the label reads zero rather
/// than wrapping past the end.
#[test]
fn test_tail_cursor_empty() {
    let app = composition::app();
    assert!(matches!(app.event_cursor_at_tail(), EventCursor::Local(0)));
}

/// A durable anchor resolves by identity, not index: the front of the log
/// draining shifts every resident frame, and the anchored entry is still
/// found where it now sits. An anchor the drain took resolves to nothing,
/// and the count reports itself as a floor.
#[test]
fn test_cursor_across_front_drain() {
    let mut app = composition::app();
    app.sessions
        .current
        .transcript
        .push_frame(stamped_frame("e1", "first"));
    app.sessions
        .current
        .transcript
        .push_frame(stamped_frame("e2", "second"));
    app.sessions
        .current
        .transcript
        .push_frame(stamped_frame("e3", "third"));
    app.ui.transcript_view.unseen_since = Some(EventCursor::Durable("e2".into()));
    assert_eq!(
        app.new_turn_count().count,
        1,
        "the third entry is the one unseen turn"
    );

    let total = app.sessions.current.transcript.resident_bytes();
    let freed = app.sessions.current.transcript.frames()[0].estimated_bytes() as u64;
    assert_eq!(
        app.sessions
            .current
            .transcript
            .drain_front_to_budget(total - freed),
        1,
        "the drain takes the first entry alone"
    );
    assert_eq!(
        app.new_turn_count().count,
        1,
        "the anchor resolves by identity after the drain shifted it"
    );
    assert!(!app.new_turn_count().is_lower_bound);

    app.ui.transcript_view.unseen_since = Some(EventCursor::Durable("e1".into()));
    assert!(
        app.new_turn_count().is_lower_bound,
        "an anchor the drain took leaves the count a floor"
    );
}

/// A durable cursor resolves to the frames past the anchored entry. An
/// anchor whose frame is no longer resident counts the whole window and
/// reports the count as a floor, since the frames it stood on are gone.
#[test]
fn test_cursor_resolves_durable_anchor() {
    let mut app = composition::app();
    app.sessions
        .current
        .transcript
        .push_frame(stamped_frame("e1", "first"));
    app.sessions
        .current
        .transcript
        .push_frame(stamped_frame("e2", "second"));

    app.ui.transcript_view.unseen_since = Some(EventCursor::Durable("e1".into()));
    assert_eq!(
        app.new_turn_count().count,
        1,
        "an anchor on the first entry leaves the second turn to count"
    );
    assert!(!app.new_turn_count().is_lower_bound);

    app.ui.transcript_view.unseen_since = Some(EventCursor::Durable("absent".into()));
    assert_eq!(
        app.new_turn_count().count,
        1,
        "an anchor off the resident range counts the window it still shows"
    );
    assert!(
        app.new_turn_count().is_lower_bound,
        "the count is a floor when the anchor frame is gone"
    );
}
