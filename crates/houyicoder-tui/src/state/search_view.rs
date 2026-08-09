//! Full-screen search view entry/exit, split from state.rs so the parent
//! stays under the file-size gate. The view is Scroll mode with verbose render
//! and a query; the verbose transcript IS the result set, so there is no
//! list/detail projection to drift from the index (the structural fix for the
//! preview/detail desync bugs).

use super::App;

impl App {
    /// Enter Scroll mode, remembering the current viewport so Esc/End returns
    /// to it. Triggered by PgUp.
    pub fn enter_scroll(&mut self) {
        self.prev_viewport = self.viewport;
        self.viewport = crate::state::ViewportMode::Scroll;
    }

    /// Enter the full-screen search view: Scroll mode with verbose render and
    /// the query, frozen at the current transcript tail (the newest match
    /// lands in view). The popup/inline surfaces stay untouched (dead until
    /// their deletion) so this view owns no list/detail -- the verbose
    /// transcript IS the result, which is what closes the preview/detail drift
    /// bugs structurally.
    pub fn enter_search_view(&mut self, query: &str) {
        self.enter_scroll();
        self.verbose = true;
        // Load the snapshot from the durable log via the seam when wired;
        // else fall back to a clone of the live transcript (the 4000-row
        // cap still bounds the live vec, so the clone is the legacy path).
        // Under the threshold: load the whole log into the snapshot vec.
        // Over the threshold: open byte-window mode -- one screen at a time,
        // flat-rendered (no fold slots), so peak resident stays bounded
        // regardless of log size. NOT a degrade: the window IS the real
        // search surface; the chrome shows byte-% position, not "too large".
        if let Some(snap) = self.snapshot.as_ref() {
            self.snapshot_log_bytes = snap.log_size();
            if self.snapshot_log_bytes > crate::scroll::SEARCH_LOG_THRESHOLD {
                self.window_mode = true;
                self.frozen_file_size = self.snapshot_log_bytes;
                let w = snap.tail_window(crate::scroll::WINDOW_MAX_BYTES);
                self.search_transcript = w.lines;
                self.window_anchor = w.start_offset;
                self.window_end = w.next_offset;
                self.search_skipped = w.skipped;
                self.window_skipped = w.skipped;
                self.search_truncated = false;
                self.window_scroll = crate::scroll::WindowScroll::default();
            } else {
                let load = snap.load(crate::scroll::SEARCH_LOG_THRESHOLD);
                self.search_transcript = load.lines;
                self.search_truncated = load.truncated;
                self.search_skipped = load.skipped;
            }
        } else {
            self.search_transcript = self.transcript.clone();
            self.search_truncated = false;
            self.snapshot_log_bytes = 0;
            self.search_skipped = 0;
        }
        self.search.active = true;
        self.search.query = query.to_string();
        self.run_search();
        // Newest first: matches are in transcript order (oldest->newest), so
        // the newest is the last index. Opening there matches the "find the
        // most recent discussion of X" intent.
        if !self.search.matches.is_empty() {
            self.search.focus = self.search.matches.len() - 1;
            self.jump_to_focused_match();
        }
    }

    /// Leave the search view: clear verbose + in_search + the query, then
    /// restore the viewport active before scrolling.
    pub fn exit_search_view(&mut self) {
        self.verbose = false;
        self.search.active = false;
        self.search.input_mode = false;
        self.search.query.clear();
        self.search.matches.clear();
        self.search.input.clear();
        self.search.focus = 0;
        self.search_transcript.clear();
        self.search_truncated = false;
        self.snapshot_log_bytes = 0;
        self.search_skipped = 0;
        self.window_mode = false;
        self.window_anchor = 0;
        self.window_end = 0;
        self.frozen_file_size = 0;
        self.window_skipped = 0;
        self.window_scroll = crate::scroll::WindowScroll::default();
        self.indexing.set(false);
        self.indexed_bytes.set(0);
        self.index_total.set(0);
        self.index_done.set(false);
        self.exit_scroll();
    }

    /// Enter the in-view slash re-search input bar. Seeds the buffer with
    /// the current query (less-style: slash shows the last pattern,
    /// editable). The query/matches/focus are NOT touched until Enter
    /// commits, so Esc cancel is a trivial no-op restore -- no snapshot
    /// to save and roll back.
    pub fn enter_search_input(&mut self) {
        self.search.input.set(self.search.query.clone());
        self.search.input_mode = true;
    }

    /// Commit the in-view slash bar: the buffer becomes the new query,
    /// matches re-run, focus lands on the newest match (the find-the-most-
    /// recent-discussion-of-X intent, same as /search entry), and the
    /// viewport jumps to it. An empty buffer yields no matches (chrome
    /// shows no-match); the view stays open so the user can retry.
    pub fn commit_search_input(&mut self) {
        self.search.query = self.search.input.value().to_string();
        self.search.input.clear();
        self.search.input_mode = false;
        self.run_search();
        if !self.search.matches.is_empty() {
            self.search.focus = self.search.matches.len() - 1;
            self.jump_to_focused_match();
        }
    }

    /// Cancel the in-view slash bar: discard the buffer, keep the prior
    /// query and matches untouched. The view stays in search mode at its
    /// prior position.
    pub fn cancel_search_input(&mut self) {
        self.search.input.clear();
        self.search.input_mode = false;
    }

    /// The transcript the view currently renders + counts against: the
    /// frozen snapshot while the search view is open, else the live
    /// transcript. Every count path (fold_aware_rows, transcript_row_of_line)
    /// and every render path (display_slots, line indexing, the focused
    /// match range) reads this, so count==render holds under the snapshot
    /// without a synced pair -- the third time this bug class would have
    /// bitten (the first two were the verbose-count desync + the live/disk
    /// split), and the accessor is the structural fix.
    pub fn active_transcript(&self) -> &[crate::records::TranscriptLine] {
        if self.search.active {
            &self.search_transcript
        } else {
            &self.transcript
        }
    }

    /// Run the search against active_transcript. The borrow needs the
    /// disjoint-field form (the match slice borrows search_transcript while
    /// search.run borrows search mutably); a method returning active_transcript
    /// would over-borrow self, so the branching stays inline here.
    pub fn run_search(&mut self) {
        let t: &[crate::records::TranscriptLine] = if self.search.active {
            &self.search_transcript
        } else {
            &self.transcript
        };
        self.search.run(t);
    }

    /// Advance the focused search match and scroll the transcript to it.
    pub fn search_next_and_jump(&mut self) {
        self.search.next();
        self.jump_to_focused_match();
    }

    /// Step the focused search match backward and scroll the transcript to it.
    pub fn search_prev_and_jump(&mut self) {
        self.search.prev();
        self.jump_to_focused_match();
    }

    /// Jump the focused search match (used by Enter in both surfaces). In
    /// byte-window mode the row math is flat (no fold) and the scroll state
    /// is window_scroll -- separate from the whole-vec TranscriptScroll.
    pub fn jump_to_focused_match(&mut self) {
        if let Some(idx) = self.search.focused_line() {
            if self.window_mode {
                let row = self.flat_row_of_line(idx);
                self.window_scroll.jump_to(row);
            } else {
                let row = self.transcript_row_of_line(idx);
                self.transcript_scroll.jump_to(row);
            }
        }
    }

    /// n in byte-window mode: walk toward the older match. If the current
    /// window holds an older match, the in-window prev handles it. Otherwise
    /// load prior windows (reverse) searching for the next older match,
    /// bounding the scan so a sparse-match log does not stall on one keypress.
    /// Bounded, not infinite: a bounded scan that finds nothing stays put
    /// (the chrome shows the partial total) -- honest, not a false "no more".
    pub fn window_search_older(&mut self) {
        if !self.window_mode {
            self.search.prev();
            self.jump_to_focused_match();
            return;
        }
        // In-window older match: the current match is not the oldest in this
        // window (focus > 0).
        if self.search.focus > 0 {
            self.search.prev();
            self.jump_to_focused_match();
            return;
        }
        let Some(snap) = self.snapshot.clone() else {
            return;
        };
        // Scan up to BOUND windows older; focus the NEWEST match in the first
        // window that has one (the closest older match).
        const BOUND: usize = 64;
        let mut anchor = self.window_anchor;
        for _ in 0..BOUND {
            if anchor == 0 {
                return; // BOF: no older window.
            }
            let w = snap.window_before(anchor, crate::scroll::WINDOW_MAX_BYTES);
            if w.lines.is_empty() || w.start_offset == anchor {
                return; // no progress (corrupt/empty) -- stop.
            }
            let new_anchor = w.start_offset;
            self.search_transcript = w.lines;
            self.window_anchor = new_anchor;
            self.window_end = w.next_offset;
            self.window_skipped = w.skipped;
            self.window_scroll = crate::scroll::WindowScroll::default();
            self.run_search();
            if !self.search.matches.is_empty() {
                // Newest match in this older window = last index.
                self.search.focus = self.search.matches.len() - 1;
                self.jump_to_focused_match();
                return;
            }
            anchor = new_anchor;
        }
        // Bounded scan exhausted: stay (the chrome's partial total signals it).
    }

    /// N in byte-window mode: walk toward the newer match. If the current
    /// window holds a newer match, the in-window next handles it. Otherwise
    /// load newer windows (forward) searching for the next newer match.
    pub fn window_search_newer(&mut self) {
        if !self.window_mode {
            self.search.next();
            self.jump_to_focused_match();
            return;
        }
        // In-window newer match: current match is not the newest (focus < n-1).
        if !self.search.matches.is_empty() && self.search.focus + 1 < self.search.matches.len() {
            self.search.next();
            self.jump_to_focused_match();
            return;
        }
        let Some(snap) = self.snapshot.clone() else {
            return;
        };
        const BOUND: usize = 64;
        let mut cursor = self.window_end;
        for _ in 0..BOUND {
            if cursor >= self.frozen_file_size {
                return; // EOF: no newer window.
            }
            let w = snap.window(cursor, crate::scroll::WINDOW_MAX_BYTES);
            if w.lines.is_empty() || w.next_offset == cursor {
                return; // no progress -- stop.
            }
            let new_end = w.next_offset;
            self.search_transcript = w.lines;
            self.window_anchor = w.start_offset;
            self.window_end = new_end;
            self.window_skipped = w.skipped;
            self.window_scroll = crate::scroll::WindowScroll::default();
            self.run_search();
            if !self.search.matches.is_empty() {
                // Newest match in a forward (older->newer) window = last index,
                // but N walks toward newer so the FIRST match is the closest
                // newer one.
                self.search.focus = 0;
                self.jump_to_focused_match();
                return;
            }
            cursor = new_end;
        }
    }

    /// G in byte-window mode: start the full event-byte-offset index build
    /// (one chunk per frame; Esc interrupts). The render path pumps
    /// index_chunk while indexing is set.
    pub fn start_full_index(&mut self) {
        if self.window_mode {
            self.indexing.set(true);
            self.index_done.set(false);
        }
    }

    /// Esc while indexing interrupts the build (the partial index is kept --
    /// a later G resumes from where it left off). Returns true if it
    /// consumed the Esc so the caller does not also exit the view.
    pub fn interrupt_index(&mut self) -> bool {
        if self.indexing.get() {
            self.indexing.set(false);
            true
        } else {
            false
        }
    }

    /// Pump one index chunk (called from the flat render path each frame while
    /// indexing). Stops when the build completes; publishes progress to the
    /// Cells the status bar reads.
    pub fn pump_index_chunk(&self) {
        let Some(snap) = self.snapshot.as_ref() else {
            return;
        };
        if !self.indexing.get() {
            return;
        }
        let p = snap.index_chunk();
        self.indexed_bytes.set(p.indexed_bytes);
        self.index_total.set(p.total_bytes);
        if p.done {
            self.index_done.set(true);
            self.indexing.set(false);
        }
    }
}
