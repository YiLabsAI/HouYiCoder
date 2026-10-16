//! Transcript scroll methods on App, split from state.rs so that file stays
//! under the file-size gate. The steps live here rather than on the scroll
//! state because a step names the row it lands on, and the row list belongs to
//! the App. All page/line scroll goes through the view state's scroll; the
//! debug_scroll helper is private to this impl block and only these methods
//! call it.

use crate::row_id::RowId;
use crate::rows::row_set::RowSet;
use crate::scroll::{NewTurnCount, ScrollAnchor, ScrollTransition, TranscriptScroll};
use crate::state::{App, EventCursor};
use crate::transcript::TranscriptFrame;
use crate::view::working::history_window::window_rows;
use crate::view::working::transcript::ensure_cached_slots;
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
        self.ensure_scroll_rows();
    }

    /// Build the rows a step names its landing row from, when the rows on file
    /// are not the ones for the row list now resident. A row that cannot be
    /// named cannot be stood on: the walk a draw runs is what builds them, and
    /// that draw then finds them cached. The width is the one the last frame
    /// stashed, so a step taken before the first frame builds nothing and
    /// leaves the viewport following the tail -- no key arrives before a frame
    /// in a running terminal. The byte window is the exception: the jump that
    /// opens the search view runs in the same step that loads the window, so
    /// that view builds its rows here.
    fn ensure_scroll_rows(&self) {
        let rect = self.ui.frame.layout.transcript_rect.get();
        if rect.width == 0 {
            return;
        }
        if self.window_browsing() {
            self.ensure_window_rows(rect.width);
            return;
        }
        ensure_cached_slots(rect, self);
    }

    /// Build the byte window's rows when the frame's build no longer describes
    /// the window on view. The entry load and the older/newer walk replace the
    /// window without a frame in between, so a step that lands right after
    /// would otherwise name its row in the list of the window it replaced. The
    /// build is the one the next frame runs, over the same lines at the same
    /// width, so the row it names is the row the reader will see.
    fn ensure_window_rows(&self, width: u16) {
        let key = (self.window_anchor(), self.window_end(), width);
        if self.ui.frame.layout.window_rows_key.get() == Some(key) {
            return;
        }
        let rows = window_rows(self, width);
        *self.ui.frame.layout.last_window_rows.borrow_mut() = rows;
        self.ui.frame.layout.window_rows_key.set(Some(key));
    }

    /// Read the full row set of the view on screen: the rows a step names and
    /// the rows an anchor resolves against. The transcript views keep theirs in
    /// the render cache, keyed by the view; a byte window builds its rows
    /// every frame, so the last build is what a step reads.
    fn with_scroll_rows<R>(&self, read: impl FnOnce(&RowSet) -> R) -> R {
        if self.window_browsing() {
            read(&self.ui.frame.layout.last_window_rows.borrow())
        } else {
            self.ui.renderer.slots.with(read)
        }
    }

    /// Park the viewport on the row standing at this screen row, naming that
    /// row by identity so the place survives the list being rebuilt. A view
    /// whose rows carry no name at all has no place to hold, so it keeps
    /// following the tail.
    fn park_at_row(&mut self, row: usize) {
        match self.with_scroll_rows(|rows| rows.anchor_at(row)) {
            Some(anchor) => {
                let scroll = self.active_scroll_mut();
                scroll.park(anchor);
                scroll.set_resolved_top(row);
            }
            None => self.active_scroll_mut().follow_tail(),
        }
    }

    /// The highest screen row a viewport can stand on and still fill: beyond it
    /// the view is at the tail and follows it.
    fn max_scroll_top(&self) -> usize {
        let scroll = self.active_scroll();
        scroll.total.get().saturating_sub(scroll.effective_cap())
    }

    /// Move the viewport to this screen row: park on it, or return to the tail
    /// when it is the tail or below. A transcript that fits one viewport has no
    /// place to hold, so a step over it keeps following rather than parking on
    /// the only screen there is -- otherwise a wheel on a short session would
    /// park and show a jump-to-bottom label over a view already at the bottom.
    fn step_scroll_to(&mut self, row: usize) -> ScrollTransition {
        if !self.active_scroll().has_scroll_room() {
            return ScrollTransition::Unchanged;
        }
        if row >= self.max_scroll_top() {
            if self.active_scroll().is_following_tail() {
                return ScrollTransition::Unchanged;
            }
            self.active_scroll_mut().follow_tail();
            return ScrollTransition::ReachedTail;
        }
        self.park_at_row(row);
        ScrollTransition::Parked
    }

    /// Park the transcript on view at this screen row, naming the row so a
    /// rebuild between the jump and the next draw resolves back to it. The
    /// search match jump and the top-of-view jump both land here.
    pub fn park_scroll_at(&mut self, row: usize) {
        self.ensure_rows_published();
        self.park_at_row(row);
    }

    /// Step the byte-window view, which keeps no scroll-away snapshot and
    /// trims nothing: it renders a frozen range of the log rather than the
    /// running turn, so the parent's bookkeeping has nothing to say about it.
    fn window_step(&mut self, up: bool, rows: usize) {
        self.ensure_rows_published();
        let before = self.active_scroll().top_offset();
        let row = if up {
            before.saturating_sub(rows)
        } else {
            before.saturating_add(rows)
        };
        self.step_scroll_to(row);
    }

    /// Step the byte-window view up by n lines.
    pub fn window_line_up(&mut self, n: usize) {
        self.window_step(true, n);
    }

    /// Step the byte-window view down by n lines.
    pub fn window_line_down(&mut self, n: usize) {
        self.window_step(false, n);
    }

    /// Page the byte-window view up one viewport.
    pub fn window_page_up(&mut self) {
        let cap = self.active_scroll().effective_cap();
        self.window_step(true, cap);
    }

    /// Page the byte-window view down one viewport.
    pub fn window_page_down(&mut self) {
        let cap = self.active_scroll().effective_cap();
        self.window_step(false, cap);
    }

    /// Resolve the parked anchor of the scroll on view against the rows that view
    /// renders, and publish the screen row it stands on. An anchor whose row is
    /// not resident keeps the last published row, so the view holds its place
    /// until the older read that brings the row back lands and this runs again.
    ///
    /// The caller must have built the rows of the on-view scroll (a draw does;
    /// a rebuild reaches the whole-vec form through settle_whole_scroll), so
    /// the anchor is resolved against the list it belongs to.
    pub(crate) fn settle_scroll(&self) {
        let Some(anchor) = self.active_scroll().anchor().cloned() else {
            return;
        };
        if let Some(row) = self.resolved_row(&anchor) {
            // The published row is a cell, so settling from the read-only draw
            // does not need the mutable accessor the steps use.
            self.active_scroll().set_resolved_top(row);
        }
    }

    /// Resolve the whole-vec transcript's parked anchor after a pass that
    /// changed its rows, and publish the screen row it stands on. A pass that
    /// adds or releases rows above the viewport moves the row the viewport
    /// stands on, and the anchor names that row rather than a number the pass
    /// would patch up by how many rows it moved.
    ///
    /// Skipped while another view owns the resident rows: the child renders
    /// its own transcript and the byte window its loaded range, so resolving
    /// the parent's anchor then would read the wrong list. The parent settles
    /// at its next draw instead, which re-resolves by identity.
    pub(crate) fn settle_whole_scroll(&self) {
        if self.ui.teammate_view.is_some() || self.window_browsing() {
            return;
        }
        if self.whole_scroll().anchor().is_none() {
            return;
        }
        // The pass invalidated the rows on file, so build the current ones
        // before resolving against them.
        self.ensure_scroll_rows();
        self.settle_scroll();
    }

    /// The screen row this anchor stands on among the rows now resident, or
    /// None when no row carries its identity.
    fn resolved_row(&self, anchor: &ScrollAnchor) -> Option<usize> {
        if let Some(row) = self.with_scroll_rows(|rows| rows.row_of(anchor)) {
            return Some(row);
        }
        // A collapsed group renders as its own row instead of the lines inside
        // it, so an anchor on one of those lines stands on the group's row. The
        // byte window folds nothing, so its rows carry every line's own name.
        if self.window_browsing() {
            return None;
        }
        let line = self.line_of_identity(&anchor.row)?;
        Some(self.fold_aware_rows(Some(line)))
    }

    /// The line this identity names in the transcript on view, for an anchor
    /// whose line a collapsed group stands for.
    fn line_of_identity(&self, id: &RowId) -> Option<usize> {
        if self.ui.teammate_view.is_some() || self.ui.transcript_view.search_active() {
            return id.local_position().map(|position| position as usize);
        }
        self.sessions
            .current
            .transcript
            .row_ids()
            .iter()
            .position(|row| row == id)
    }

    /// The identity of the transcript line at this position on the view on
    /// screen: the store's own name for a session line, and the line's position
    /// for a transcript assembled outside it -- a child's rows, a search
    /// snapshot -- which is the rule those lists are named by everywhere.
    pub(crate) fn line_identity(&self, index: usize) -> RowId {
        if self.ui.teammate_view.is_some() || self.ui.transcript_view.search_active() {
            return RowId::local(index as u64);
        }
        self.sessions
            .current
            .transcript
            .row_id(index)
            .cloned()
            .unwrap_or_else(|| RowId::local(index as u64))
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
        let cap = self.active_scroll().effective_cap();
        self.step_scroll_to(before.saturating_sub(cap));
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
        let cap = self.active_scroll().effective_cap();
        let transition = self.step_scroll_to(before.saturating_add(cap));
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

    /// Let the whole-vec viewport go with the history it was standing in. A
    /// pass that replaces the session history (the one on view, not merely a
    /// longer list under it) leaves the parked row out of the log instead of
    /// off screen, so the anchor can never resolve again and the viewport
    /// returns to the tail instead of holding a row the fresh session does not
    /// have. The reader is told when they had parked, because the place they
    /// left is gone rather than moved.
    ///
    /// Every pass that advances the history generation ends here, after the
    /// rows it installs: releasing against a list that is still the old one
    /// would drop an anchor that the new list could have resolved.
    pub(crate) fn release_history_view(&mut self) {
        let released = self.whole_scroll().anchor().is_some();
        self.whole_scroll_mut().follow_tail();
        self.resume_tail_trim();
        if released {
            self.system_line("scroll position released: the history on view was replaced");
        }
    }

    /// Break follow-tail while keeping the current top row on screen. Callers
    /// that expand a line in place need this: the row list grows under the
    /// viewport, and a viewport left following the tail would move the reader
    /// to the bottom with it. Parking on the row already at the top holds the
    /// reader's place instead, and names it, so the rebuild that follows the
    /// expansion resolves back to the same row.
    pub fn pin_transcript_top(&mut self) {
        let top = self.active_scroll().top_offset();
        self.park_at_row(top);
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
        self.step_scroll_to(before.saturating_sub(n));
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
        let transition = self.step_scroll_to(before.saturating_add(n));
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
