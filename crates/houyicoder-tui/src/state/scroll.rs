//! Transcript scroll methods on App, split from state.rs so that file stays
//! under the file-size gate. All page/line scroll goes through the view
//! state's scroll; the debug_scroll helper is private to this impl block and
//! only these methods call it.

use crate::row_id::RowId;
use crate::scroll::{NewTurnCount, ScrollTransition, TranscriptScroll};
use crate::state::{App, EventCursor};
use crate::transcript::TranscriptFrame;
use houyicoder_protocol::frontend::session_update::SessionUpdate;

impl App {
    /// The scroll state of the view on screen: the teammate's own while its
    /// view is open, otherwise the active transcript view's -- the window's
    /// scroll while the search view browses a byte window, the whole-vec
    /// transcript's otherwise. Presentation paths (draw, keys, mouse,
    /// status) read this; parent-only domain work (rebuild, trim, history
    /// reads) reads the whole-vec scroll directly and never this.
    pub(crate) fn active_scroll(&self) -> &TranscriptScroll {
        match &self.ui.teammate_view {
            Some(view) => &view.scroll,
            None => self.ui.transcript_view.scroll_on_view(),
        }
    }

    /// The mutable form of active_scroll, for the page and line scroll
    /// commands. Callers must not run parent-only side effects
    /// (snapshot_scroll_away, resume_tail_trim) off this when a child is on
    /// view; the scroll commands below branch those by surface instead.
    pub(crate) fn active_scroll_mut(&mut self) -> &mut TranscriptScroll {
        match &mut self.ui.teammate_view {
            Some(view) => &mut view.scroll,
            None => self.ui.transcript_view.scroll_on_view_mut(),
        }
    }

    /// Publish the row count into the on-view scroll before any scroll step,
    /// so the step clamps against the same total the renderer publishes.
    /// Before the first render the cell holds zero; the walk is the fallback
    /// then, close to what a first draw would publish (it counts unwrapped
    /// rows, so content that would wrap is undercounted until that draw).
    /// After the first render the cell is the count==render single source
    /// and the walk is skipped.
    pub(crate) fn ensure_rows_published(&mut self) {
        if self.active_scroll().total.get() == 0 {
            let walked = self.fold_aware_rows(None);
            self.active_scroll_mut().total.set(walked);
        }
    }

    /// Page the transcript on view up by one viewport (older rows). Only the
    /// parent's scroll breaks follow-tail into a new-message count; the
    /// child's scroll moves alone.
    pub fn scroll_transcript_up(&mut self) {
        self.ensure_rows_published();
        let total = self.transcript_display_rows();
        let viewing_child = self.ui.teammate_view.is_some();
        let before = self.active_scroll().top_offset();
        let was_following = self.active_scroll().is_following_tail();
        self.active_scroll_mut().page_up();
        if !viewing_child {
            self.snapshot_scroll_away(was_following);
        }
        let after = self.active_scroll().top_offset();
        self.debug_scroll("up", total, before, after);
    }

    /// Page the transcript on view down by one viewport (newer rows). A step
    /// that crosses to the tail trims the live transcript and clears the
    /// scroll-away snapshot so the cap stays aligned and the new-message label
    /// dismisses. Only the parent trims: the child surface owns no trim, so a
    /// child step to the tail must not cut parent history the reader never
    /// scrolled away from.
    pub fn scroll_transcript_down(&mut self) {
        self.ensure_rows_published();
        let total = self.transcript_display_rows();
        let viewing_child = self.ui.teammate_view.is_some();
        let before = self.active_scroll().top_offset();
        let transition = self.active_scroll_mut().page_down();
        if !viewing_child && transition == ScrollTransition::ReachedTail {
            self.resume_tail_trim();
        }
        let after = self.active_scroll().top_offset();
        self.debug_scroll("down", total, before, after);
    }

    /// Return the transcript on view to following the tail. On the parent
    /// this also clears the scroll-away snapshot so the next scroll-back
    /// starts a fresh "new messages" count (an on-repin clears the unseen
    /// divider), and trims the live transcript so the cap holds at the tail.
    /// On the child only the child's scroll follows; the parent keeps its
    /// position and its cap. Called by the jump-to-bottom label click, a new
    /// user submission, End, Ctrl+End, and PageDown-to-bottom.
    pub fn scroll_transcript_follow_tail(&mut self) {
        let viewing_child = self.ui.teammate_view.is_some();
        self.active_scroll_mut().follow_tail();
        if !viewing_child {
            self.resume_tail_trim();
        }
    }

    /// Break follow-tail while keeping the current top row on screen. Callers
    /// that expand a line in place need this: clearing the follow flag alone
    /// falls back to the last pinned offset, which is 0 on a session that
    /// never scrolled, so the view jumps to the top of the transcript.
    pub fn pin_transcript_top(&mut self) {
        let top = self.active_scroll().top_offset();
        self.active_scroll_mut().jump_to(top);
    }

    /// Capture the frame index the first time a scroll breaks follow-tail
    /// this scroll-back session. The null guard preserves the original
    /// baseline across subsequent scroll actions (a second wheel-up must not
    /// reset the count). No-op when the scroll did not break follow (short
    /// transcript, max_top==0) or when a snapshot already exists.
    fn snapshot_scroll_away(&mut self, was_following: bool) {
        if was_following
            && !self.whole_scroll().is_following_tail()
            && self.ui.transcript_view.unseen_since.is_none()
        {
            self.ui.transcript_view.unseen_since = Some(self.event_cursor_at_tail());
        }
    }

    /// Clear the scroll-away snapshot and trim the live transcript now that
    /// the viewport is back at the tail. The version bump fires only when the
    /// trim actually dropped lines, so a no-op return to the tail does not
    /// invalidate the render cache.
    fn resume_tail_trim(&mut self) {
        self.ui.transcript_view.unseen_since = None;
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
        let Some(cursor) = self.ui.transcript_view.unseen_since.as_ref() else {
            return NewTurnCount::default();
        };
        let (from, is_lower_bound) = match self.frame_index_for_cursor(cursor) {
            Some(from) => (from, false),
            None => (0, true),
        };
        let mut count = 0usize;
        let mut prev_was_agent = false;
        for f in &self.sessions.current.transcript.frames()[from..] {
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
    /// Anchors Durable on the tail frame's row identity when the frame carries
    /// one, else Local on the absolute frame count, which a front drain does
    /// not shift. Both resolve to the frame count, so the label reads zero new
    /// turns at the moment of capture.
    fn event_cursor_at_tail(&self) -> EventCursor {
        let abs = self.sessions.current.transcript.abs_frame_count() as u64;
        match self.sessions.current.transcript.frames().last() {
            // An unstamped tail keeps the boundary one past the last frame,
            // which is what the resolver shifts onto the resident range. The
            // local argument is not read here: a stamped tail takes the
            // durable form, and an unstamped one falls through to the frame
            // count, so no counter value of ours could reach the cursor.
            Some(sf) => match RowId::of(sf.as_ref(), sf.event_id.as_ref(), 0) {
                id @ RowId::Durable { .. } => EventCursor::Durable(id),
                _ => EventCursor::Local(abs),
            },
            None => EventCursor::Local(0),
        }
    }

    /// Resolve a cursor to the index of the first resident frame past its
    /// anchor: the count of frames already consumed. Durable finds the frame
    /// its identity names and steps past it; Local is an absolute frame index,
    /// shifted onto the resident range. None when the anchor's frame is no
    /// longer resident, so the caller counts the whole window instead of
    /// reading it as empty.
    fn frame_index_for_cursor(&self, cursor: &EventCursor) -> Option<usize> {
        let base = self.sessions.current.transcript.frame_window_start();
        match cursor {
            EventCursor::Durable(id) => self
                .sessions
                .current
                .transcript
                .frames()
                .iter()
                .position(|sf| id.names(sf.as_ref(), sf.event_id.as_ref()))
                .map(|i| i + 1),
            EventCursor::Local(n) => {
                let abs = *n as usize;
                (abs >= base)
                    .then(|| (abs - base).min(self.sessions.current.transcript.frame_count()))
            }
        }
    }

    /// Step the transcript on view up by n lines (wheel = 3, edge
    /// auto-scroll = 1). A line step keeps continuity with the prior
    /// viewport, unlike a full page jump. Only the parent's scroll breaks
    /// follow-tail into a new-message count.
    pub fn scroll_transcript_line_up(&mut self, n: usize) {
        self.ensure_rows_published();
        let total = self.transcript_display_rows();
        let viewing_child = self.ui.teammate_view.is_some();
        let before = self.active_scroll().top_offset();
        let was_following = self.active_scroll().is_following_tail();
        self.active_scroll_mut().line_up(n);
        if !viewing_child {
            self.snapshot_scroll_away(was_following);
        }
        let after = self.active_scroll().top_offset();
        self.debug_scroll("line-up", total, before, after);
    }

    /// Step the transcript on view down by n lines. See
    /// scroll_transcript_line_up. A parent step that crosses to the tail
    /// trims and clears the scroll-away snapshot, matching page_down; a
    /// child step to the tail moves the child scroll alone.
    pub fn scroll_transcript_line_down(&mut self, n: usize) {
        self.ensure_rows_published();
        let total = self.transcript_display_rows();
        let viewing_child = self.ui.teammate_view.is_some();
        let before = self.active_scroll().top_offset();
        let transition = self.active_scroll_mut().line_down(n);
        if !viewing_child && transition == ScrollTransition::ReachedTail {
            self.resume_tail_trim();
        }
        let after = self.active_scroll().top_offset();
        self.debug_scroll("line-down", total, before, after);
    }

    /// Env-gated (HOUYICODER_DEBUG_LOG file) scroll trace: the step delta
    /// tells whether a wheel event pages a full viewport (delta == cap, the
    /// "full replace" experience) or steps a few lines (native continuity).
    /// When the env is unset this is a no-op. Traces the surface the user is
    /// scrolling, not the parent.
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
            follow = self.active_scroll().is_following_tail(),
            "scroll"
        );
    }
}

#[cfg(test)]
mod tests;
