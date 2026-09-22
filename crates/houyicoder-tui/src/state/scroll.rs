//! Transcript scroll methods on App, split from state.rs so that file stays
//! under the file-size gate. All page/line scroll goes through the
//! transcript_scroll field; the debug_scroll helper is private to this impl
//! block and only these methods call it.

use crate::scroll::ScrollTransition;
use crate::state::App;

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
            && self.scrolled_from_frame.is_none()
        {
            self.scrolled_from_frame = Some(self.transcript.frame_count());
        }
    }

    /// Clear the scroll-away snapshot and trim the live transcript now that
    /// the viewport is back at the tail. The version bump fires only when the
    /// trim actually dropped lines, so a no-op return to the tail does not
    /// invalidate the render cache.
    fn resume_tail_trim(&mut self) {
        self.scrolled_from_frame = None;
        let dropped = self.trim_live_transcript();
        if dropped > 0 {
            self.bump_transcript_version();
        }
    }

    /// Number of new agent turns since the user scrolled away from the tail
    /// — the N in the "N new messages" pill. Zero while following the tail.
    ///
    /// One turn counts once, however many agent chunks, tool calls, or
    /// thoughts it contains: only a user message resets prev_was_agent, so
    /// the count follows turn boundaries rather than frame arrivals. The
    /// snapshot is a frame index rather than a transcript length, so the cap
    /// that drops the oldest pushed rows cannot silently zero the count, and
    /// it is clamped in case a rewind truncated frames below it.
    pub fn jump_pill_new_count(&self) -> usize {
        let Some(from) = self.scrolled_from_frame else {
            return 0;
        };
        let from = from.min(self.transcript.frame_count());
        let mut count = 0usize;
        let mut prev_was_agent = false;
        for f in &self.transcript.frames()[from..] {
            match f.as_ref() {
                // Turn boundary: a new user message starts a new assistant
                // turn, so the next agent text counts again.
                crate::transcript::TranscriptFrame::Session(
                    houyicoder_protocol::frontend::session_update::SessionUpdate::UserMessageChunk(
                        _,
                    ),
                ) => prev_was_agent = false,
                crate::transcript::TranscriptFrame::Session(
                    houyicoder_protocol::frontend::session_update::SessionUpdate::AgentMessageChunk(
                        _,
                    ),
                ) => {
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
        count
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
