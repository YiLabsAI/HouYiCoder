//! Transcript scroll methods on App, split from state.rs so that file stays
//! under the file-size gate. All page/line scroll goes through the
//! transcript_scroll field; the debug_scroll helper is private to this impl
//! block and only these methods call it.

use crate::scroll::{NewTurnCount, ScrollTransition};
use crate::state::{App, EventCursor};
use crate::transcript::TranscriptFrame;
use houyicoder_protocol::frontend::session_update::SessionUpdate;

impl App {
    /// Page the transcript up by one viewport (older rows).
    pub fn scroll_transcript_up(&mut self) {
        let total = self.transcript_display_rows();
        let before = self.transcript_scroll.top_offset(total);
        let was_following = self.transcript_scroll.is_following_tail();
        self.transcript_scroll.page_up(total);
        self.snapshot_scroll_away(was_following);
        let after = self.transcript_scroll.top_offset(total);
        self.debug_scroll("up", total, before, after);
    }

    /// Page the transcript down by one viewport (newer rows). A step that
    /// crosses to the tail trims the live transcript and clears the
    /// scroll-away snapshot so the cap stays aligned and the new-message pill
    /// dismisses.
    pub fn scroll_transcript_down(&mut self) {
        let total = self.transcript_display_rows();
        let before = self.transcript_scroll.top_offset(total);
        if self.transcript_scroll.page_down(total) == ScrollTransition::ReachedTail {
            self.resume_tail_trim();
        }
        let after = self.transcript_scroll.top_offset(total);
        self.debug_scroll("down", total, before, after);
    }

    /// Return the transcript scroll to following the tail. Also clears the
    /// scroll-away snapshot so the next scroll-back starts a fresh "new
    /// messages" count (an on-repin clears the unseen divider), and trims the
    /// live transcript so the cap holds at the tail. Called by the
    /// jump-to-bottom pill click, a new user submission, End, Ctrl+End, and
    /// PageDown-to-bottom.
    pub fn scroll_transcript_follow_tail(&mut self) {
        self.transcript_scroll.follow_tail();
        self.resume_tail_trim();
    }

    /// Break follow-tail while keeping the current top row on screen. Callers
    /// that expand a line in place need this: clearing the follow flag alone
    /// falls back to the last pinned offset, which is 0 on a session that
    /// never scrolled, so the view jumps to the top of the transcript.
    pub fn pin_transcript_top(&mut self) {
        let total = self.transcript_scroll.total.get();
        let top = self.transcript_scroll.top_offset(total);
        self.transcript_scroll.jump_to(top);
    }

    /// Capture the frame index the first time a scroll breaks follow-tail
    /// this scroll-back session. The null guard preserves the original
    /// baseline across subsequent scroll actions (a second wheel-up must not
    /// reset the count). No-op when the scroll did not break follow (short
    /// transcript, max_top==0) or when a snapshot already exists.
    fn snapshot_scroll_away(&mut self, was_following: bool) {
        if was_following
            && !self.transcript_scroll.is_following_tail()
            && self.unseen_since.is_none()
        {
            self.unseen_since = Some(self.event_cursor_at_tail());
        }
    }

    /// Clear the scroll-away snapshot and trim the live transcript now that
    /// the viewport is back at the tail. The version bump fires only when the
    /// trim actually dropped lines, so a no-op return to the tail does not
    /// invalidate the render cache.
    fn resume_tail_trim(&mut self) {
        self.unseen_since = None;
        let dropped = self.trim_live_transcript();
        if dropped > 0 {
            self.bump_transcript_version();
        }
    }

    /// Number of new agent turns since the user scrolled away from the tail
    /// — the N in the "N new messages" label. Zero while following the tail.
    ///
    /// One turn counts once, however many agent chunks, tool calls, or
    /// thoughts it contains: only a user message resets prev_was_agent, so
    /// the count follows turn boundaries rather than frame arrivals. The
    /// baseline is an absolute or durable cursor rather than a transcript
    /// length, so the cap that drops the oldest pushed rows does not silently
    /// zero the count.
    pub fn new_turn_count(&self) -> NewTurnCount {
        let Some(cursor) = self.unseen_since else {
            return NewTurnCount::default();
        };
        let (from, is_lower_bound) = match self.frame_index_for_cursor(cursor) {
            Some(from) => (from, false),
            None => (0, true),
        };
        let mut count = 0usize;
        let mut prev_was_agent = false;
        for f in &self.transcript.frames()[from..] {
            match f.as_ref() {
                // Turn boundary: a new user message starts a new assistant
                // turn, so the next agent text counts again.
                TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_)) => {
                    prev_was_agent = false;
                }
                TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(_)) => {
                    if !prev_was_agent {
                        count += 1;
                    }
                    prev_was_agent = true;
                }
                // Tool / thought / other frames do NOT reset prev_was_agent,
                // so a tool call within one turn does not start a new count.
                _ => {}
            }
        }
        NewTurnCount {
            count,
            is_lower_bound,
        }
    }

    /// The cursor at the current tail: the boundary one past the last frame.
    /// Anchors Server on the tail frame's event seq when the frame carries
    /// one, else Local on the absolute frame count, which a front drain does
    /// not shift. Both resolve to the frame count, so the label reads zero new
    /// turns at the moment of capture.
    fn event_cursor_at_tail(&self) -> EventCursor {
        match self.transcript.frames().last() {
            Some(sf) => match sf.seq {
                Some(seq) => EventCursor::Server(seq),
                None => EventCursor::Local(self.transcript.abs_frame_count() as u64),
            },
            None => EventCursor::Local(0),
        }
    }

    /// Resolve a cursor to the index of the first resident frame past its
    /// anchor: the count of frames already consumed. Server finds the anchored
    /// seq and steps past it; Local is an absolute frame index, shifted onto
    /// the resident range. None when the anchor's frame is no longer resident,
    /// so the caller counts the whole window instead of reading it as empty.
    fn frame_index_for_cursor(&self, cursor: EventCursor) -> Option<usize> {
        let base = self.transcript.frame_window_start();
        match cursor {
            EventCursor::Server(seq) => self
                .transcript
                .frames()
                .iter()
                .position(|sf| sf.seq == Some(seq))
                .map(|i| i + 1),
            EventCursor::Local(n) => {
                let abs = n as usize;
                (abs >= base).then(|| (abs - base).min(self.transcript.frame_count()))
            }
        }
    }

    /// Step the transcript up by n lines (wheel = 3, edge auto-scroll = 1).
    /// A line step keeps continuity with the prior viewport, unlike a full
    /// page jump.
    pub fn scroll_transcript_line_up(&mut self, n: usize) {
        let total = self.transcript_display_rows();
        let before = self.transcript_scroll.top_offset(total);
        let was_following = self.transcript_scroll.is_following_tail();
        self.transcript_scroll.line_up(n, total);
        self.snapshot_scroll_away(was_following);
        let after = self.transcript_scroll.top_offset(total);
        self.debug_scroll("line-up", total, before, after);
    }

    /// Step the transcript down by n lines. See scroll_transcript_line_up.
    /// A step that crosses to the tail trims and clears the scroll-away
    /// snapshot, matching page_down.
    pub fn scroll_transcript_line_down(&mut self, n: usize) {
        let total = self.transcript_display_rows();
        let before = self.transcript_scroll.top_offset(total);
        if self.transcript_scroll.line_down(n, total) == ScrollTransition::ReachedTail {
            self.resume_tail_trim();
        }
        let after = self.transcript_scroll.top_offset(total);
        self.debug_scroll("line-down", total, before, after);
    }

    /// Env-gated (HOUYICODER_DEBUG_LOG file) scroll trace: the step delta
    /// tells whether a wheel event pages a full viewport (delta == cap, the
    /// "full replace" experience) or steps a few lines (native continuity).
    /// When the env is unset this is a no-op.
    fn debug_scroll(&self, dir: &str, total: usize, before: usize, after: usize) {
        let delta = after
            .saturating_sub(before)
            .max(before.saturating_sub(after));
        tracing::debug!(
            dir,
            total,
            before,
            after,
            delta,
            follow = self.transcript_scroll.is_following_tail(),
            "scroll"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composition;
    use crate::transcript::SequencedFrame;
    use houyicoder_protocol::envelope::EventSeq;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::ContentChunk;

    fn agent_frame(seq: u64, text: &str) -> SequencedFrame {
        SequencedFrame {
            seq: Some(EventSeq(seq)),
            frame: TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::Text { text: text.into() },
            ))),
        }
    }

    /// A tail frame carrying a server seq anchors the cursor on that seq, so
    /// the baseline survives a front drain that a raw frame index would not.
    #[test]
    fn test_tail_cursor_server_anchored() {
        let mut app = composition::app();
        app.transcript.push_frame(agent_frame(5, "first"));
        assert!(matches!(
            app.event_cursor_at_tail(),
            EventCursor::Server(EventSeq(5))
        ));
    }

    /// A tail frame with no server seq anchors Local on the absolute frame
    /// count, which a front drain does not shift.
    #[test]
    fn test_tail_cursor_local_count() {
        let mut app = composition::app();
        app.transcript
            .push_frame(TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text { text: "hi".into() }),
            )));
        assert!(matches!(
            app.event_cursor_at_tail(),
            EventCursor::Local(n) if n == 1
        ));
    }

    /// An empty transcript anchors Local(0) so the pill reads zero rather
    /// than wrapping past the end.
    #[test]
    fn test_tail_cursor_empty() {
        let app = composition::app();
        assert!(matches!(app.event_cursor_at_tail(), EventCursor::Local(0)));
    }

    /// A server cursor resolves to the frames past the anchored seq. An anchor
    /// whose frame is no longer resident counts the whole window and reports
    /// the count as a floor, since the frames it stood on are gone.
    #[test]
    fn test_cursor_resolves_server_anchor() {
        let mut app = composition::app();
        app.transcript.push_frame(agent_frame(5, "first"));
        app.transcript.push_frame(agent_frame(6, "second"));

        app.unseen_since = Some(EventCursor::Server(EventSeq(5)));
        assert_eq!(
            app.new_turn_count().count,
            1,
            "server anchor on seq 5 leaves the seq 6 turn to count"
        );
        assert!(!app.new_turn_count().is_lower_bound);

        app.unseen_since = Some(EventCursor::Server(EventSeq(999)));
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
}
