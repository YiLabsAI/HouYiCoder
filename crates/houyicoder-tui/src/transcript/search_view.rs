//! Full-screen search view entry/exit, split from state.rs so the parent
//! stays under the file-size gate. The view is Scroll mode with verbose render
//! and a query; the verbose transcript IS the result set, so there is no
//! list/detail view to drift from the index (the structural fix for the
//! preview/detail desync bugs).

use std::time::Duration;

use crate::composition::shared_runtime;
use crate::notifications::{NotifKind, Notification};
use crate::records::TranscriptLine;
use crate::scroll::SEARCH_LOG_THRESHOLD;
use crate::scroll::WINDOW_MAX_BYTES;
use crate::state::App;
use crate::state::ViewportMode;
use crate::state::scan_chunk::{PendingScanChunk, SCAN_CHUNK_BUDGET, ScanChunkPoll};
use crate::transcript::snapshot::ScanProgress;
use crate::transcript::snapshot::{SnapshotLoad, WindowLoad};
use std::sync::mpsc::channel;

impl App {
    /// Enter Scroll mode, remembering the current viewport so Esc/End returns
    /// to it. Triggered by PgUp.
    pub fn enter_scroll(&mut self) {
        self.ui.navigation.prev_viewport = self.ui.navigation.viewport;
        self.ui.navigation.viewport = ViewportMode::Scroll;
    }

    /// Enter the full-screen search view: Scroll mode with verbose render and
    /// the query, frozen at the current transcript tail (the newest match
    /// lands in view). The popup/inline surfaces stay untouched (dead until
    /// their deletion) so this view owns no list/detail -- the verbose
    /// transcript IS the result, which is what closes the preview/detail drift
    /// bugs structurally.
    ///
    /// Refused while a child view is open: the search reads the parent
    /// session's durable log, so its matches would count against the parent
    /// while the child's transcript renders, and a match jump would move the
    /// hidden parent's viewport. The refusal says why instead of staying
    /// silent; searching a child's own log is a separate capability.
    pub fn enter_search_view(&mut self, query: &str) {
        if self.ui.teammate_view.is_some() {
            self.ui.notifications.add(Notification::immediate(
                "search-blocked-teammate",
                NotifKind::Text {
                    text: "shift+↑↓ to exit the teammate view, then search".to_string(),
                    color: None,
                },
                Duration::from_millis(2000),
            ));
            return;
        }
        self.enter_scroll();
        self.ui.verbose = true;
        // Load the snapshot from the durable log via the seam when wired;
        // else fall back to a clone of the live transcript (the 4000-row
        // cap still bounds the live vec, so the clone is the legacy path).
        // Under the threshold: load the whole log into the snapshot vec.
        // Over the threshold: open byte-window mode -- one screen at a time,
        // flat-rendered (no fold slots), so peak resident stays bounded
        // regardless of log size. NOT a degrade: the window IS the real
        // search surface; the chrome shows byte-% position, not "too large".
        // Collect the load inside the snapshot borrow, then commit the whole
        // enter transaction once the borrow ends -- the fields land together
        // or not at all.
        enum Loaded {
            Over { log_bytes: u64, w: WindowLoad },
            Under { load: SnapshotLoad },
        }
        let loaded = self.sessions.persistence.snapshot.as_ref().map(|snap| {
            let log_bytes = snap.log_size();
            if log_bytes > SEARCH_LOG_THRESHOLD {
                let w = snap.tail_window(WINDOW_MAX_BYTES).unwrap_or_else(|e| {
                    tracing::warn!("search tail window read failed: {e:?}");
                    WindowLoad::default()
                });
                Loaded::Over { log_bytes, w }
            } else {
                let load = snap.load(SEARCH_LOG_THRESHOLD);
                Loaded::Under { load }
            }
        });
        match loaded {
            Some(Loaded::Over { log_bytes, w }) => {
                self.ui.transcript_view.enter_search_over_window(
                    w.lines,
                    log_bytes,
                    w.skipped,
                    w.start_offset,
                    w.next_offset,
                    query,
                );
            }
            Some(Loaded::Under { load }) => {
                self.ui.transcript_view.enter_search_under(
                    load.lines,
                    load.truncated,
                    load.skipped,
                    query,
                );
            }
            None => {
                let live = self.sessions.current.transcript.lines().to_vec();
                self.ui.transcript_view.enter_search_fallback(live, query);
            }
        }
        if self.ui.transcript_view.search_focus_newest() {
            self.jump_to_focused_match();
        }
    }

    /// Leave the search view: clear verbose + in_search + the query, then
    /// restore the viewport active before scrolling.
    pub fn exit_search_view(&mut self) {
        self.ui.verbose = false;
        self.ui.transcript_view.exit_search();
        self.sessions.current.scan_state.indexing.set(false);
        self.sessions.current.scan_state.scanned_bytes.set(0);
        self.sessions.current.scan_state.scan_total.set(0);
        self.exit_scroll();
    }

    /// Enter the in-view slash re-search input bar. Seeds the buffer with
    /// the current query (less-style: slash shows the last pattern,
    /// editable). The query/matches/focus are NOT touched until Enter
    /// commits, so Esc cancel is a trivial no-op restore -- no snapshot
    /// to save and roll back.
    pub fn enter_search_input(&mut self) {
        self.ui.transcript_view.begin_search_input();
    }

    /// Commit the in-view slash bar: the buffer becomes the new query,
    /// matches re-run, focus lands on the newest match (the find-the-most-
    /// recent-discussion-of-X intent, same as /search entry), and the
    /// viewport jumps to it. An empty buffer yields no matches (chrome
    /// shows no-match); the view stays open so the user can retry.
    pub fn commit_search_input(&mut self) {
        self.ui.transcript_view.commit_search_input();
        if self.ui.transcript_view.search_focus_newest() {
            self.jump_to_focused_match();
        }
    }

    /// Cancel the in-view slash bar: discard the buffer, keep the prior
    /// query and matches untouched. The view stays in search mode at its
    /// prior position.
    pub fn cancel_search_input(&mut self) {
        self.ui.transcript_view.cancel_search_input();
    }

    /// The transcript the view currently renders + counts against: the
    /// frozen snapshot while the search view is open, else the live
    /// transcript. Every count path (fold_aware_rows, transcript_row_of_line)
    /// and every render path (display_slots, line indexing, the focused
    /// match range) reads this, so count==render holds under the snapshot
    /// without a synced pair -- the third time this bug class would have
    /// bitten (the first two were the verbose-count desync + the live/disk
    /// split), and the accessor is the structural fix.
    pub fn active_transcript(&self) -> &[TranscriptLine] {
        if let Some(tv) = &self.ui.teammate_view {
            &tv.transcript
        } else if self.ui.transcript_view.search_active() {
            self.ui.transcript_view.search_corpus_lines()
        } else {
            self.sessions.current.transcript.lines()
        }
    }

    /// Run the search against the active transcript.
    pub fn run_search(&mut self) {
        if self.ui.transcript_view.search_active() {
            self.ui.transcript_view.run_search_on_corpus();
        } else {
            let lines = self.sessions.current.transcript.lines();
            self.ui.transcript_view.run_search_on_lines(lines);
        }
    }

    /// Advance the focused search match and scroll the transcript to it.
    pub fn search_next_and_jump(&mut self) {
        self.ui.transcript_view.search_next();
        self.jump_to_focused_match();
    }

    /// Step the focused search match backward and scroll the transcript to it.
    pub fn search_prev_and_jump(&mut self) {
        self.ui.transcript_view.search_prev();
        self.jump_to_focused_match();
    }

    /// Jump the focused search match (used by Enter in both surfaces). In
    /// byte-window mode the row math is flat (no fold) and the scroll state
    /// is window_scroll -- separate from the whole-vec TranscriptScroll.
    pub fn jump_to_focused_match(&mut self) {
        if let Some(idx) = self.ui.transcript_view.search_focused_line() {
            if self.window_browsing() {
                let row = self.history_row_of_line(idx);
                self.window_scroll_mut().jump_to(row);
            } else {
                let row = self.transcript_row_of_line(idx);
                self.whole_scroll_mut().jump_to(row);
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
        if !self.window_browsing() {
            self.ui.transcript_view.search_prev();
            self.jump_to_focused_match();
            return;
        }
        // In-window older match: the current match is not the oldest in this
        // window (focus > 0).
        if self.ui.transcript_view.search_focus() > 0 {
            self.ui.transcript_view.search_prev();
            self.jump_to_focused_match();
            return;
        }
        let Some(snap) = self.sessions.persistence.snapshot.clone() else {
            return;
        };
        // Scan up to BOUND windows older; focus the NEWEST match in the first
        // window that has one (the closest older match).
        const BOUND: usize = 64;
        let mut anchor = self.window_anchor();
        for _ in 0..BOUND {
            if anchor == 0 {
                return; // BOF: no older window.
            }
            let w = match snap.window_before(anchor, WINDOW_MAX_BYTES) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!("search older window read failed: {e:?}");
                    return;
                }
            };
            if w.lines.is_empty() || w.start_offset == anchor {
                return; // no progress (corrupt/empty) -- stop.
            }
            let new_anchor = w.start_offset;
            self.ui.transcript_view.replace_search_window(
                w.lines,
                new_anchor,
                w.next_offset,
                w.skipped,
            );
            if self.ui.transcript_view.search_matches_len() > 0 {
                // Newest match in this older window = last index.
                self.ui.transcript_view.search_focus_last();
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
        if !self.window_browsing() {
            self.ui.transcript_view.search_next();
            self.jump_to_focused_match();
            return;
        }
        // In-window newer match: current match is not the newest (focus < n-1).
        if self.ui.transcript_view.search_matches_len() > 0
            && self.ui.transcript_view.search_focus() + 1
                < self.ui.transcript_view.search_matches_len()
        {
            self.ui.transcript_view.search_next();
            self.jump_to_focused_match();
            return;
        }
        let Some(snap) = self.sessions.persistence.snapshot.clone() else {
            return;
        };
        const BOUND: usize = 64;
        let mut cursor = self.window_end();
        for _ in 0..BOUND {
            if cursor >= self.frozen_file_size() {
                return; // EOF: no newer window.
            }
            let w = snap.window(cursor, WINDOW_MAX_BYTES);
            if w.lines.is_empty() || w.next_offset == cursor {
                return; // no progress -- stop.
            }
            let new_end = w.next_offset;
            self.ui.transcript_view.replace_search_window(
                w.lines,
                w.start_offset,
                new_end,
                w.skipped,
            );
            if self.ui.transcript_view.search_matches_len() > 0 {
                // Newest match in a forward (older->newer) window = last index,
                // but N walks toward newer so the FIRST match is the closest
                // newer one.
                self.ui.transcript_view.search_focus_first();
                self.jump_to_focused_match();
                return;
            }
            cursor = new_end;
        }
    }

    /// How many scan chunks are outstanding: dispatched and not yet applied.
    ///
    /// Within the scan a record lasts exactly as long as its chunk -- the pump
    /// clears it on a landed or dead one, an interrupt deliberately leaves it on
    /// file -- so counting records bounds chunk reads. A history read cannot
    /// reason this way: its invalidation clears the record under a worker still
    /// inside the log, which is why that budget is held by the worker.
    fn pending_scan_chunks(&self) -> usize {
        usize::from(
            self.sessions
                .current
                .scan_state
                .pending_scan_chunk
                .borrow()
                .is_some(),
        )
    }

    /// G in byte-window mode: start the full epoch scan (one chunk at a time;
    /// Esc interrupts). The first chunk is dispatched here, and the loop
    /// advances the scan from there.
    ///
    /// A scan already running is left to the loop. The key carries no gate of
    /// its own, so pressing it twice would dispatch twice, and the second
    /// dispatch drops the first chunk's receiver: its worker reads the log and
    /// sends into a channel nobody holds.
    pub fn start_full_scan(&mut self) {
        if !self.window_browsing() {
            return;
        }
        self.sessions.current.scan_state.indexing.set(true);
        if self.pending_scan_chunks() >= SCAN_CHUNK_BUDGET {
            return;
        }
        self.dispatch_scan_chunk();
    }

    /// Esc while scanning interrupts it (the partial pass is kept -- a later G
    /// resumes from where it left off). Returns true if it consumed the Esc
    /// so the caller does not also exit the view.
    ///
    /// The chunk already on its worker is not cancelled and cannot be: it is a
    /// blocking read of the log. It lands, its progress applies, and no next
    /// chunk is dispatched, so one chunk is the worst case an interrupt leaves
    /// running. That bound is the scan's whole stop contract, because a worker
    /// here reads one chunk and returns rather than walking the log.
    pub fn interrupt_scan(&mut self) -> bool {
        if self.sessions.current.scan_state.indexing.get() {
            self.sessions.current.scan_state.indexing.set(false);
            true
        } else {
            false
        }
    }

    /// Advance the epoch scan by one step, on the loop rather than on a draw:
    /// a chunk reads the log, and the draw path must not.
    ///
    /// A wired runtime reads the chunk on a worker and applies it when it
    /// lands; the bare path, which has no runtime, reads it inline. Returns
    /// true when progress moved, so the caller redraws the chrome.
    ///
    /// This is the scan's stop point. A scan nobody wants dispatches no next
    /// chunk, and the loop advances the scan from here, so an interrupt takes
    /// effect at the next chunk boundary rather than partway through a read.
    pub fn pump_scan_chunk(&mut self) -> bool {
        let mut moved = self.apply_scan_progress();
        if !self.sessions.current.scan_state.indexing.get() {
            return moved;
        }
        if self.pending_scan_chunks() >= SCAN_CHUNK_BUDGET {
            // A chunk is already running; the next step waits for it.
            return moved;
        }
        if self.sessions.current.connection.is_some() {
            self.dispatch_scan_chunk();
            return moved;
        }
        // No connection: the read happens where it stands instead, which is what
        // the bare and test compositions exercise.
        if let Some(snap) = self.sessions.persistence.snapshot.clone() {
            let p = snap.scan_chunk();
            self.apply_scan_chunk(p);
            moved = true;
        }
        moved
    }

    /// Dispatch one scan chunk on a worker, when a runtime is wired.
    ///
    /// The one place a chunk task is started, so the budget is asserted here:
    /// every caller guards before asking, and a caller that forgets is told in
    /// debug rather than leaving two workers reading one log with only the
    /// second one's receiver on file.
    fn dispatch_scan_chunk(&mut self) {
        debug_assert!(
            self.pending_scan_chunks() < SCAN_CHUNK_BUDGET,
            "a scan chunk was dispatched while {} were outstanding, past the budget of {}",
            self.pending_scan_chunks(),
            SCAN_CHUNK_BUDGET
        );
        if self.sessions.current.connection.is_none() {
            return;
        }
        let Some(snap) = self.sessions.persistence.snapshot.clone() else {
            return;
        };
        let (tx, rx) = channel();
        shared_runtime().spawn_blocking(move || {
            tx.send(snap.scan_chunk()).ok();
        });
        *self
            .sessions
            .current
            .scan_state
            .pending_scan_chunk
            .borrow_mut() = Some(PendingScanChunk::new(rx));
    }

    /// Apply a chunk that landed on the worker. Returns true when it moved the
    /// scan forward.
    fn apply_scan_progress(&mut self) -> bool {
        let polled = self
            .sessions
            .current
            .scan_state
            .pending_scan_chunk
            .borrow()
            .as_ref()
            .map(PendingScanChunk::poll);
        match polled {
            Some(ScanChunkPoll::Ready(progress)) => {
                *self
                    .sessions
                    .current
                    .scan_state
                    .pending_scan_chunk
                    .borrow_mut() = None;
                self.apply_scan_chunk(progress);
                true
            }
            Some(ScanChunkPoll::Disconnected) => {
                // The worker died without sending: stop claiming to scan, so
                // a dead task cannot stall the scan forever.
                *self
                    .sessions
                    .current
                    .scan_state
                    .pending_scan_chunk
                    .borrow_mut() = None;
                self.sessions.current.scan_state.indexing.set(false);
                true
            }
            Some(ScanChunkPoll::Pending) | None => false,
        }
    }

    /// Publish a chunk's progress to the cells the chrome reads, and stop the
    /// scan when the chunk says it reached the beginning of the log.
    fn apply_scan_chunk(&self, progress: ScanProgress) {
        self.sessions
            .current
            .scan_state
            .scanned_bytes
            .set(progress.scanned_bytes);
        self.sessions
            .current
            .scan_state
            .scan_total
            .set(progress.total_bytes);
        if progress.done {
            self.sessions.current.scan_state.indexing.set(false);
        }
    }
}

#[cfg(test)]
mod tests;
