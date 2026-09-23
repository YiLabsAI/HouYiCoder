//! Incremental transcript rebuilding from the ordered frame log.
//!
//! The visible history is bounded, and the stable prefix is reused while new
//! frames extend the current turn. Rewind and scrollback loading invalidate only
//! the affected range, keeping rebuild cost independent of session length.

use std::ops::Range;

use super::history_read::{OVERLAP_FRONT_ROWS, ReadJob};
use super::{MAX_REBUILD_FRAMES, PREPEND_BATCH};
use crate::records::TranscriptLine;
use crate::state::App;
use crate::state::transcript::DiskFront;
use crate::state::transcript::blocks::{
    Block, BlockAnchor, BlockId, TranscriptChange, TranscriptChangeSet,
};
use crate::state::transcript::{HistoryReadOutcome, HistoryReadPoll, PendingHistoryRead};
use crate::transcript::TranscriptFrame;
use crate::transcript::transcript_from_frames_at;

/// Rows the view holds from the session log before it stops reading older
/// ones: the same bound the viewable transcript keeps. The bound is checked
/// before a read rather than after, so one read can pass it, and by however
/// many rows a window of the log byte budget holds, which no row count limits.
const LOG_ROW_BUDGET: usize = crate::scroll::VIEWABLE_SCROLLBACK_CAP;

impl App {
    /// Rebuild the transcript from the frame log. Each turn is one block whose
    /// identity (the event seq of the frame that opened it, or a log position
    /// for a frontend-raised run) survives rebuild, so a turn whose frames did
    /// not change carries its derived lines and expand state forward without
    /// re-deriving them or re-pairing them by content. Only the turns whose
    /// frame range changed re-derive; a rewind truncates the stale tail.
    pub(crate) fn rebuild_transcript(&mut self) {
        let turn_start = self.current_turn_start();
        let newest_open = self.run_state.is_active();
        let frame_start = self.visible_frame_start();
        let frame_end = self.transcript.abs_frame_count();
        // Rows read back from the session log sit above the capped view while
        // the reader is at the tail, which is the only view that cannot reach
        // them. They leave with it, so the next scroll back reads afresh. They
        // also leave when the resident front moved past the rows they were
        // read to sit above: the frames between the two fronts are drained, so
        // holding them would leave a hole in the scrollback.
        if self.transcript_scroll.is_following_tail()
            || self.transcript.disk_rows_front() != self.transcript.frame_window_start()
        {
            self.transcript.clear_disk_rows();
        }
        // The frozen prefix is one block re-derived as a whole when it grows
        // (a new turn moves turn_start forward, so the prior active run joins
        // it); the active turn is the pair-kept run from turn_start to the log
        // end, which current_turn_start may span interjections to keep a call
        // beside its result. Two blocks suffice: the absorbed prior active run
        // is always the tail, so a rewind drops it without a middle insert.
        let has_frozen = turn_start > frame_start;
        let has_active = turn_start < frame_end;
        let mut ranges: Vec<Range<usize>> = Vec::new();
        if has_frozen {
            ranges.push(frame_start..turn_start);
        }
        if has_active {
            ranges.push(turn_start..frame_end);
        }

        let (changes, clear_all, prefix_reused) =
            self.build_block_changes(&ranges, frame_start, has_active, newest_open);
        if clear_all {
            self.transcript.blocks_mut().clear();
        }
        self.transcript.blocks_mut().apply(changes);

        // Flatten the in-window blocks to the viewable line list. A block whose
        // frames sit before the window start (the frame cap, or a drain in an
        // earlier pass) is skipped here; the drain below runs after this, so the
        // rows it reclaims still render in the pass that drops them. The rows
        // read back from the session log lead the list: they are older than
        // every block, so the current turn starts past them.
        let mut lines: Vec<TranscriptLine> = Vec::new();
        let mut prefix_line_count = self.transcript.disk_row_count();
        lines.extend_from_slice(self.transcript.disk_rows());
        for b in self.transcript.blocks().blocks() {
            if b.frame_range.end <= frame_start || b.frame_range.start >= frame_end {
                continue;
            }
            if b.frame_range.end <= turn_start {
                prefix_line_count += b.lines.len();
            }
            lines.extend_from_slice(&b.lines);
        }
        self.transcript.replace_lines(lines);
        self.transcript.current_turn_mut().line_index = prefix_line_count;
        self.update_fold_cache(prefix_line_count, prefix_reused);

        // Every resident frame is scanned before the drain drops any of them,
        // so a drained frame's verdicts are already in the audit cache.
        self.accumulate_wire_state();
        self.enforce_frame_byte_budget(turn_start);
        // A drain in this pass moved the resident front past the frame the
        // loaded rows were cut at, so the rows leave in the same pass: the
        // frames between the two fronts are gone, and rows above a front that
        // does not continue them would render a gap.
        if self.transcript.disk_row_count() > 0
            && self.transcript.disk_rows_front() != self.transcript.frame_window_start()
        {
            let released = self.transcript.clear_disk_rows();
            if !self.transcript_scroll.is_following_tail() {
                let top = self.transcript_scroll.raw_top();
                self.transcript_scroll
                    .set_raw_top(top.saturating_sub(released));
            }
        }
        self.trim_live_transcript();
        self.bump_transcript_version();
    }

    /// Walk the target ranges against the current block list, matching each by
    /// stable id. A reused frozen prefix keeps its lines and fold groups; an
    /// active turn whose frames grew re-derives; a cap advance or a new turn
    /// replaces a slot in place; a truncated log rewinds the tail. Returns the
    /// change set, whether the whole list must clear first, and whether the
    /// frozen prefix was reused (so the fold cache rescans only the tail).
    fn build_block_changes(
        &self,
        ranges: &[Range<usize>],
        frame_start: usize,
        has_active: bool,
        newest_open: bool,
    ) -> (TranscriptChangeSet, bool, bool) {
        let mut changes = TranscriptChangeSet::default();
        let mut cur_idx = 0;
        let mut clear_all = false;
        let mut prefix_reused = false;
        let blocks = self.transcript.blocks().blocks();
        // Evict blocks the cap advanced past: a block whose frame range ended
        // before the window start is out of view, so name the first survivor
        // for EvictBefore to drain the aged-out prefix. When no block
        // survives the advance, the whole list is cleared before re-derive.
        while cur_idx < blocks.len() && blocks[cur_idx].frame_range.end <= frame_start {
            cur_idx += 1;
        }
        if cur_idx > 0 {
            if let Some(survivor) = blocks.get(cur_idx) {
                changes.push(TranscriptChange::EvictBefore { id: survivor.id });
            } else {
                clear_all = true;
            }
        }
        for (ri, range) in ranges.iter().enumerate() {
            let is_active = has_active && ri + 1 == ranges.len();
            // The frozen prefix is a single reusable slot whose identity
            // stays fixed across cap advances (the window slides under it),
            // so it anchors on Local(0) rather than the moving frame_start;
            // the active turn anchors on its opening frame's seq or log
            // position, stable while the turn runs and replaced in place
            // when a new turn takes the active slot.
            let id = if is_active {
                let anchor_seq = self
                    .transcript
                    .resident_frame(range.start)
                    .and_then(|sf| sf.seq);
                self.transcript.blocks().assign_id(anchor_seq, range.start)
            } else {
                BlockId(BlockAnchor::Local(0))
            };
            let slot = blocks.get(cur_idx);
            let matched = slot.is_some_and(|b| b.id == id);
            if matched && !is_active && slot.expect("checked").frame_range == *range {
                // Unchanged frozen prefix: keep its lines and fold groups
                // as they are, so only the tail re-derives.
                prefix_reused = true;
                cur_idx += 1;
                continue;
            }
            let lines = transcript_from_frames_at(
                self.transcript.frames(),
                self.transcript.frame_window_start(),
                range.clone(),
                newest_open && is_active,
            );
            let revision = slot.map(|b| b.revision.wrapping_add(1)).unwrap_or(0);
            let block = Block {
                id,
                frame_range: range.clone(),
                lines,
                revision,
            };
            match slot {
                // Slot holds a block whose identity no longer fits the
                // range (the cap slid the frozen window, or a new turn
                // took the active slot): replace it in place by its old
                // identity so the slot order is preserved without a rewind.
                Some(old) if !matched => {
                    changes.push(TranscriptChange::ReplaceBlock { id: old.id, block });
                    cur_idx += 1;
                }
                Some(_) => {
                    changes.push(TranscriptChange::ReplaceBlock { id, block });
                    cur_idx += 1;
                }
                None => {
                    changes.push(TranscriptChange::AppendBlock(block));
                }
            }
        }
        // A truncated frame log left blocks past the last range: drop them
        // back to the last block the walk kept.
        if cur_idx < blocks.len()
            && let Some(last_kept) = cur_idx.checked_sub(1).and_then(|i| blocks.get(i))
        {
            changes.push(TranscriptChange::RewindTo { id: last_kept.id });
        }
        (changes, clear_all, prefix_reused)
    }

    /// Recompute the fold cache after a rebuild. When the frozen prefix was
    /// reused its lines and groups are unchanged, so only the active tail is
    /// rescanned; otherwise a turn grew or the window slid and everything is
    /// recomputed. The cache is keyed by absolute line index, so the split
    /// point is the prefix's line count.
    fn update_fold_cache(&mut self, prefix_line_count: usize, prefix_reused: bool) {
        use crate::fold::{compute_fold_groups, fold_groups_in};
        use std::collections::HashMap;
        let agent_busy = self.agent_busy();
        let groups = if prefix_reused {
            let mut groups = std::mem::take(self.transcript.fold_groups_mut());
            // A group that sits before the current turn boundary is in a
            // completed turn and never active. Count its calls per id first
            // so the tail continues each ordinal instead of reusing a key
            // when a call id recurs across turns.
            groups.retain(|g| g.start < prefix_line_count);
            let mut ordinal: HashMap<String, u32> = HashMap::new();
            for g in &mut groups {
                *ordinal.entry(g.call_id.clone()).or_insert(0) += 1;
                g.active = false;
            }
            groups.extend(fold_groups_in(
                &self.transcript.lines()[prefix_line_count..],
                prefix_line_count,
                agent_busy,
                &mut ordinal,
            ));
            groups
        } else {
            compute_fold_groups(self.transcript.lines(), agent_busy)
        };
        *self.transcript.fold_groups_mut() = groups;
    }

    /// Recompute each group's active flag after the run pauses or resumes.
    /// The flag bakes in the agent-busy state at the last rebuild, so a
    /// Waiting transition (approval card up, no new frames) would leave it
    /// stale until the next rebuild. Rare, so the full boundary scan is fine.
    pub(crate) fn refresh_fold_active(&mut self) {
        let agent_busy = self.agent_busy();
        let threshold = crate::fold::last_turn_boundary(self.transcript.lines());
        for g in self.transcript.fold_groups_mut() {
            g.active = agent_busy && g.start >= threshold;
        }
    }

    /// Rebuild the whole visible window after a frame's payload changed in
    /// place. A reused prefix holds rows derived from the frames it covers, so
    /// a row whose frame changed under it would keep the former content until
    /// something else forced a re-derive: the boundary is set one past the log,
    /// a position no frame holds, so the next rebuild re-derives the affected
    /// block.
    pub(crate) fn rebuild_after_frame_edit(&mut self) {
        // A frame's payload changed in place (a context view refreshed, a row
        // swapped into its slot): the block covering that frame still holds the
        // former payload, and its range is unchanged so carry-forward would
        // serve it stale. Clear the blocks so the rebuild re-derives every
        // covering block from the now-current payloads. The path is rare, so
        // the re-derive cost is acceptable; the migrate pass still carries
        // fetched child rows because it runs against the cleared list's
        // successors (none) only on the tail path.
        self.transcript.blocks_mut().clear();
        self.rebuild_transcript();
    }

    /// The oldest frame the frame cap keeps in view, in absolute frame
    /// coordinates. A scrollback load may reach further back than this.
    fn capped_frame_start(&self) -> usize {
        self.transcript
            .abs_frame_count()
            .saturating_sub(MAX_REBUILD_FRAMES)
    }

    /// Return the oldest frame included in the bounded transcript history, in
    /// absolute frame coordinates. Scrollback may lower the boundary; normal
    /// rebuilds keep only the newest MAX_REBUILD_FRAMES frames. The loaded
    /// boundary is not advanced here, otherwise an initially empty session
    /// would permanently disable the cap, and it never reaches below the
    /// resident front, whose frames are gone.
    fn visible_frame_start(&self) -> usize {
        self.capped_frame_start()
            .min(self.loaded_from_frame.get())
            .max(self.transcript.frame_window_start())
    }

    /// Drain the oldest resident frames until their estimated bytes fall to
    /// the budget. The rebuild passes the active turn's start, so the user frame
    /// that opened the turn stays in view while its answer streams and a turn
    /// whose own bytes exceed the budget holds above it until it ends. A start
    /// at the resident front means the window holds no such frame, and the
    /// budget then drains the log down to its newest frame. Frames below the
    /// kept front leave the viewable window: the durable log still holds the
    /// server frames, while rows the frontend raised live only in this log.
    fn enforce_frame_byte_budget(&mut self, turn_start: usize) {
        let budget = self.transcript.resident_byte_budget() as u64;
        if self.transcript.resident_bytes() <= budget {
            return;
        }
        let keep_from = if turn_start > self.transcript.frame_window_start() {
            Some(turn_start - 1)
        } else {
            None
        };
        self.transcript.drain_front_to_budget(budget, keep_from);
    }

    /// Load older history when scrollback reaches the current boundary,
    /// preserving the visible viewport position. The boundary is the oldest
    /// frame the resident window holds: below it the frames were drained, and
    /// only the session log still carries the rows they projected. The window
    /// now starts further back, so the block list is re-derived for the
    /// enlarged range.
    pub(crate) fn load_older_frames(&mut self) {
        // Don't prepend when following the tail (user is at the bottom).
        if self.transcript_scroll.is_following_tail() {
            return;
        }
        let top = self
            .transcript_scroll
            .top_offset(self.display_rows_cache.borrow().len());
        // Load only when the user is near the current history boundary.
        if top > 5 {
            return;
        }
        let from = self.visible_frame_start();
        if from <= self.transcript.frame_window_start() {
            // The frames below the resident front are gone: dispatch a
            // background read of their rows from the session log. The draw
            // path never waits on disk; the result lands on a later pump.
            self.dispatch_history_read();
            return;
        }
        // A batch reaching past the resident front can only load the frames that
        // are still present: the rest were drained, and their rows come from the
        // session log. The boundary stops at the front so the window it names
        // stays derivable.
        let batch_start = from
            .saturating_sub(PREPEND_BATCH)
            .max(self.transcript.frame_window_start());
        // Count the older batch's lines to hold the viewport steady across the
        // rebuild that follows. The batch's frames all precede the frames
        // already loaded, and a row the frontend raised among them rides the
        // batch's own projection.
        let prepended = transcript_from_frames_at(
            self.transcript.frames(),
            self.transcript.frame_window_start(),
            batch_start..from,
            self.run_state.is_active(),
        )
        .len();
        self.loaded_from_frame.set(batch_start);
        if prepended == 0 {
            return;
        }
        // Shift the scroll position down by the prepended count so the viewport
        // content stays stable. NOTE: prepended counts TranscriptLines, not
        // display rows — multi-row lines (Agent, Tool results) cause
        // under-adjustment. The next draw_transcript recomputes from the cache
        // which corrects the viewport. The one-frame drift is acceptable
        // (prepend only fires on scroll-up, the user is actively scrolling, not
        // reading a static view).
        let cur = self.transcript_scroll.raw_top();
        self.transcript_scroll.set_raw_top(cur + prepended);
        // The lowered boundary moves the window's front, so the next rebuild
        // re-derives the prefix rather than reusing it.
        self.rebuild_transcript();
        self.bump_transcript_version();
    }

    /// Dispatch a background read of older rows from the session log, the
    /// rows of the drained frames that still live on disk. The draw path
    /// never blocks on this: the task ships the result over a channel the
    /// pending slot holds, and a later pump applies it. Without a runtime or
    /// a snapshot source there is nothing to dispatch and the scroll stops at
    /// the resident front, as it did before; with a read already in flight a
    /// second dispatch is skipped so at most one runs at a time.
    fn dispatch_history_read(&mut self) {
        let Some(source) = self.snapshot.clone() else {
            return;
        };
        let Some(runtime) = self.runtime.clone() else {
            // No runtime: the bare or unwired path has no way to run a
            // background task, so the read stays off the draw thread by not
            // running at all, matching the no-source short-circuit.
            return;
        };
        if self.transcript.history_read_pending() {
            return;
        }
        if self.transcript.disk_row_count() >= LOG_ROW_BUDGET {
            return;
        }
        let job = match self.transcript.disk_front() {
            DiskFront::Stopped => return,
            DiskFront::Unloaded => {
                // The pass that drained frames still renders their rows.
                // Re-derive the view first, so the rows the overlap matches
                // are the ones that stay: a match against a row on its way
                // out would place the loaded rows above history the next
                // rebuild drops.
                self.rebuild_transcript();
                let front = self.front_row_texts();
                if front.is_empty() {
                    return;
                }
                ReadJob::Tail { source, front }
            }
            // The window ends where the loaded rows begin, so every row it
            // carries is older than every row the view holds and the overlap
            // needs no match.
            DiskFront::At(anchor) => ReadJob::Before { source, anchor },
        };
        // Capture the resident front after any rebuild the Unloaded branch ran,
        // not before: that rebuild can drain frames and advance the front, so a
        // pre-rebuild capture would make the first read's own result look stale
        // and drop it. The sync path this replaced read the front after the
        // rebuild too.
        let dispatch_front = self.transcript.frame_window_start();
        let (tx, rx) = std::sync::mpsc::channel();
        runtime.spawn_blocking(move || {
            job.run_and_send(tx);
        });
        self.transcript
            .set_history_read(PendingHistoryRead::new(dispatch_front, rx));
    }

    /// Drain the in-flight history read without blocking and apply it when
    /// ready. Returns true when a result landed and the view must redraw.
    /// Called from the loop body every pass, not inside the dirty gate, so an
    /// idle loop still picks the result up.
    pub(crate) fn pump_history_read(&mut self) -> bool {
        let Some(pending) = self.transcript.take_history_read() else {
            return false;
        };
        match pending.poll() {
            HistoryReadPoll::Pending => {
                self.transcript.set_history_read(pending);
                false
            }
            HistoryReadPoll::Ready(outcome) => {
                self.apply_history_read_result(outcome, pending.dispatch_front);
                true
            }
            HistoryReadPoll::Disconnected => {
                // The worker died or dropped its sender without sending. Drop
                // the slot and stop re-reading, so a dead task cannot pin the
                // scroll forever or block later dispatches.
                self.transcript.set_disk_front(DiskFront::Stopped);
                false
            }
        }
    }

    /// Apply a background read's result. Stale results are dropped rather than
    /// applied: if the reader returned to the tail the rows are not wanted, and
    /// if the resident front moved since dispatch the frames the rows were cut
    /// against are gone and prepending them would leave a gap. This covers
    /// tail-return and resident-front rejection; full view-generation rejection
    /// (pane, parent/child, session switch) is a later concern.
    fn apply_history_read_result(&mut self, outcome: HistoryReadOutcome, dispatch_front: usize) {
        if self.transcript_scroll.is_following_tail() {
            return;
        }
        if self.transcript.frame_window_start() != dispatch_front {
            return;
        }
        match outcome {
            HistoryReadOutcome::Exhausted => {
                self.transcript.set_disk_front(DiskFront::Stopped);
            }
            HistoryReadOutcome::Rows(result) => {
                let count = result.rows.len();
                self.transcript
                    .prepend_disk_rows(result.rows, result.anchor, dispatch_front);
                // Hold the viewport still: the rows landed above it.
                let cur = self.transcript_scroll.raw_top();
                self.transcript_scroll.set_raw_top(cur + count);
                self.rebuild_transcript();
                self.bump_transcript_version();
            }
        }
    }

    /// The rendered text of the rows the visible transcript starts at. The
    /// rows read from the log are matched against them to find where the two
    /// projections overlap.
    fn front_row_texts(&self) -> Vec<String> {
        self.transcript
            .lines()
            .iter()
            .take(OVERLAP_FRONT_ROWS)
            .map(|line| line.render())
            .collect()
    }

    /// Return the first frame in the changing turn. If that boundary would
    /// split a tool call from its result, move it backward until the pair stays
    /// together. The oldest boundary this can name is the resident front: the
    /// frames before it are drained, so a turn whose opening frame they held
    /// is re-derived from the front on the next rebuild.
    pub(crate) fn current_turn_start(&self) -> usize {
        use houyicoder_protocol::frontend::session_update::SessionUpdate;
        let base = self.transcript.frame_window_start();
        let mut search_from = self.transcript.frames().len();
        loop {
            let Some(idx) = self.transcript.frames()[..search_from]
                .iter()
                .rposition(|sf| {
                    matches!(
                        sf.as_ref(),
                        TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_))
                    )
                })
            else {
                return base;
            };
            let candidate = base + idx + 1;
            if self.prefix_has_unpaired_call(candidate) {
                // Move before this user boundary to keep the pair together.
                search_from = idx;
                continue;
            }
            return candidate;
        }
    }

    /// Whether the candidate boundary splits a tool call from a result that
    /// arrives later. A call with no result does not force the boundary back.
    /// The candidate is an absolute frame index; the scan walks the resident
    /// log from the window's front, since frames before it were drained.
    fn prefix_has_unpaired_call(&self, candidate: usize) -> bool {
        use houyicoder_protocol::frontend::session_update::SessionUpdate;
        let base = self.transcript.frame_window_start();
        let frames = self.transcript.frames();
        let at = candidate.saturating_sub(base).min(frames.len());
        let mut calls = std::collections::HashSet::new();
        let mut results = std::collections::HashSet::new();
        for sf in &frames[..at] {
            match sf.as_ref() {
                TranscriptFrame::Session(SessionUpdate::ToolCall(tc)) => {
                    calls.insert(tc.tool_call_id.0.as_str());
                }
                TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(upd)) => {
                    results.insert(upd.tool_call_id.0.as_str());
                }
                _ => {}
            }
        }
        // A tail result for a prefix call (split). Hanging calls (no result
        // anywhere) are not in tail_results, so they do not trigger.
        let mut tail_results = std::collections::HashSet::new();
        for sf in &frames[at..] {
            if let TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(upd)) = sf.as_ref() {
                tail_results.insert(upd.tool_call_id.0.as_str());
            }
        }
        calls
            .iter()
            .any(|id| !results.contains(id) && tail_results.contains(id))
    }

    /// Re-derive view caches from newly appended frames. Verdicts accumulate;
    /// todo-write applies last-write-wins only when a new checklist frame
    /// arrives, so an unrelated user boundary cannot clear active work.
    fn accumulate_wire_state(&mut self) {
        use houyicoder_protocol::acpx::AcpxMethod;
        use houyicoder_protocol::frontend::permission::PermissionDecisionEntry;
        // Verdicts are append-only (audit trail). Rewind and clear truncate
        // frames below the cursor, so the cache resets and re-parses from zero
        // to match the truncated log. A front drain leaves the absolute count
        // unchanged, so it never trips this, and the rebuild drains only after
        // this scan, so every drained frame's verdict is already cached.
        if self.verdict_cursor > self.transcript.abs_frame_count() {
            self.verdict_cursor = 0;
            self.verdict_log_cache.clear();
        }
        let base = self.transcript.frame_window_start();
        for sf in self
            .transcript
            .frames()
            .iter()
            .skip(self.verdict_cursor.saturating_sub(base))
        {
            if let TranscriptFrame::Acpx(n) = sf.as_ref()
                && matches!(n.method, AcpxMethod::ContextPermissionDecision)
                && let Ok(entry) =
                    serde_json::from_value::<PermissionDecisionEntry>(n.params.clone())
            {
                self.verdict_log_cache.push(entry);
            }
        }
        self.verdict_cursor = self.transcript.abs_frame_count();
        self.todos.update(
            self.transcript.frames(),
            self.transcript.frame_window_start(),
            self.agent_busy(),
        );
    }
}
