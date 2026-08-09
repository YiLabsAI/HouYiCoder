//! Transcript scroll methods on App, split from state.rs so that file stays
//! under the file-size gate. All page/line scroll goes through the
//! transcript_scroll field; the debug_scroll helper is private to this impl
//! block and only these methods call it.

use crate::state::App;

impl App {
    /// Page the transcript up by one viewport (older rows).
    pub fn scroll_transcript_up(&mut self) {
        let total = self.transcript_display_rows();
        let before = self.transcript_scroll.top_offset(total);
        let was_following = self.transcript_scroll.follow_tail;
        self.transcript_scroll.page_up(total);
        self.snapshot_scroll_away(was_following);
        let after = self.transcript_scroll.top_offset(total);
        self.debug_scroll("up", total, before, after);
    }

    /// Page the transcript down by one viewport (newer rows).
    pub fn scroll_transcript_down(&mut self) {
        let total = self.transcript_display_rows();
        let before = self.transcript_scroll.top_offset(total);
        let was_following = self.transcript_scroll.follow_tail;
        self.transcript_scroll.page_down(total);
        self.reset_scroll_away_if_followed(was_following);
        let after = self.transcript_scroll.top_offset(total);
        self.debug_scroll("down", total, before, after);
    }

    /// Return the transcript scroll to following the tail. Also clears the
    /// scroll-away snapshot so the next scroll-back starts a fresh "new
    /// messages" count (an on-repin clears the unseen
    /// divider). Called by the jump-to-bottom pill click, a new user
    /// submission, End, Ctrl+End, and PageDown-to-bottom.
    pub fn scroll_transcript_follow_tail(&mut self) {
        self.transcript_scroll.follow_tail();
        self.scrolled_from_frame = None;
    }

    /// Capture the frame index the first time a scroll breaks follow-tail
    /// this scroll-back session. The null guard preserves the original
    /// baseline across subsequent scroll actions (a second wheel-up must not
    /// reset the count). No-op when the scroll did not break follow (short
    /// transcript, max_top==0) or when a snapshot already exists.
    fn snapshot_scroll_away(&mut self, was_following: bool) {
        if was_following
            && !self.transcript_scroll.follow_tail
            && self.scrolled_from_frame.is_none()
        {
            self.scrolled_from_frame = Some(self.frames.len());
        }
    }

    /// Clear the scroll-away snapshot when a downward scroll reaches the
    /// tail (the user scrolled back to the bottom). Symmetric with
    /// snapshot_scroll_away so PageDown/line-down to the bottom dismisses the
    /// pill, matching End and the pill click (which go through
    /// scroll_transcript_follow_tail).
    fn reset_scroll_away_if_followed(&mut self, was_following: bool) {
        if !was_following && self.transcript_scroll.follow_tail {
            self.scrolled_from_frame = None;
        }
    }

    /// Number of new agent turns since the user scrolled away from the tail
    /// — the N in the "N new messages" pill. Counts one per user-to-assistant
    /// turn that produced agent text: prev_was_agent resets only on a new
    /// UserMessageChunk (the turn boundary), so a tool call or thought within
    /// one turn does NOT split it into N segments (the agent's "look at X" →
    /// bash → "now Y" → bash is one turn, not four). Tool-only segments
    /// (ToolCall/Result with no adjacent agent text) do not tick; one
    /// streaming response however many chunks counts as one. This is the agent
    /// content produced while the user reads history, which is the whole point
    /// of the pill; counting user-message frames instead would be dead in
    /// production (a user frame only arrives via submit, which clears the
    /// snapshot first). The snapshot is a frame index, not transcript.len(),
    /// so bound_scrollback eviction and transcript pops cannot silently zero
    /// the count. Clamps the snapshot when rewind truncated frames below it.
    /// Zero while following the tail.
    pub fn jump_pill_new_count(&self) -> usize {
        let Some(from) = self.scrolled_from_frame else {
            return 0;
        };
        let from = from.min(self.frames.len());
        let mut count = 0usize;
        let mut prev_was_agent = false;
        for f in &self.frames[from..] {
            match f {
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
        let was_following = self.transcript_scroll.follow_tail;
        self.transcript_scroll.line_up(n, total);
        self.snapshot_scroll_away(was_following);
        let after = self.transcript_scroll.top_offset(total);
        self.debug_scroll("line-up", total, before, after);
    }

    /// Step the transcript down by n lines. See scroll_transcript_line_up.
    pub fn scroll_transcript_line_down(&mut self, n: usize) {
        let total = self.transcript_display_rows();
        let before = self.transcript_scroll.top_offset(total);
        let was_following = self.transcript_scroll.follow_tail;
        self.transcript_scroll.line_down(n, total);
        self.reset_scroll_away_if_followed(was_following);
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
            follow = self.transcript_scroll.follow_tail,
            "scroll"
        );
    }
}
