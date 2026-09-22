//! Incremental transcript rebuilding from the ordered frame log.
//!
//! The visible history is bounded, and the stable prefix is reused while new
//! frames extend the current turn. Rewind and scrollback loading invalidate only
//! the affected range, keeping rebuild cost independent of session length.

use std::ops::Range;

use super::{MAX_REBUILD_FRAMES, PREPEND_BATCH};
use crate::records::TranscriptLine;
use crate::state::App;
use crate::state::transcript::blocks::{
    Block, BlockAnchor, BlockId, TranscriptChange, TranscriptChangeSet,
};
use crate::transcript::{TranscriptFrame, transcript_from_frames};

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
        let frame_end = self.transcript.frame_count();
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

        // Flatten the in-window blocks to the viewable line list. A block that
        // fell out the front when the cap advanced (frame range before the
        // window) is skipped here; frame-window eviction is a later step.
        let mut lines: Vec<TranscriptLine> = Vec::new();
        let mut prefix_line_count = 0;
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
        self.transcript.current_turn_mut().frame_index = turn_start;
        self.transcript.current_turn_mut().line_index = prefix_line_count;
        self.update_fold_cache(prefix_line_count, prefix_reused);

        self.trim_live_transcript();
        self.accumulate_wire_state();
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
                    .frames()
                    .get(range.start)
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
            let lines = transcript_from_frames(
                self.transcript.frames(),
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
        self.transcript.current_turn_mut().frame_index = self.transcript.frame_count() + 1;
        self.rebuild_transcript();
    }

    /// Return the oldest frame included in the bounded transcript history.
    /// Scrollback may lower the boundary; normal rebuilds keep only the newest
    /// MAX_REBUILD_FRAMES frames. The loaded boundary is not advanced here,
    /// otherwise an initially empty session would permanently disable the cap.
    fn visible_frame_start(&self) -> usize {
        let window = self
            .transcript
            .frame_count()
            .saturating_sub(MAX_REBUILD_FRAMES);
        window.min(self.loaded_from_frame.get())
    }

    /// Load an older frame batch when scrollback reaches the current history
    /// boundary, preserving the visible viewport position. The window now starts
    /// further back, so the block list is re-derived for the enlarged range.
    pub(crate) fn load_older_frames(&mut self) {
        let from = self.visible_frame_start();
        if from == 0 {
            return;
        }
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
        let batch_start = from.saturating_sub(PREPEND_BATCH);
        // Count the older batch's lines to hold the viewport steady across the
        // rebuild that follows. The batch's frames all precede the frames
        // already loaded, and a row the frontend raised among them rides the
        // batch's own projection.
        let prepended = transcript_from_frames(
            self.transcript.frames(),
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
        // Force a full re-derive: the enlarged window cannot match block-by-block
        // with the prior list, so set a boundary no frame holds and rebuild.
        self.transcript.current_turn_mut().frame_index = self.transcript.frame_count() + 1;
        self.rebuild_transcript();
        self.bump_transcript_version();
    }

    /// Return the first frame in the changing turn. If that boundary would
    /// split a tool call from its result, move it backward until the pair stays
    /// together.
    pub(crate) fn current_turn_start(&self) -> usize {
        use houyicoder_protocol::frontend::session_update::SessionUpdate;
        let mut search_from = self.transcript.frame_count();
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
                return 0;
            };
            let candidate = idx + 1;
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
    fn prefix_has_unpaired_call(&self, candidate: usize) -> bool {
        use houyicoder_protocol::frontend::session_update::SessionUpdate;
        let mut calls = std::collections::HashSet::new();
        let mut results = std::collections::HashSet::new();
        for sf in &self.transcript.frames()[..candidate] {
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
        for sf in &self.transcript.frames()[candidate..] {
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
        // Verdicts are append-only (audit trail). Rewind/clear truncates frames
        // below the cursor → reset + re-parse from 0 so the cache matches the
        // truncated log (no stale verdicts for dropped frames).
        if self.verdict_cursor > self.transcript.frame_count() {
            self.verdict_cursor = 0;
            self.verdict_log_cache.clear();
        }
        for sf in self.transcript.frames().iter().skip(self.verdict_cursor) {
            if let TranscriptFrame::Acpx(n) = sf.as_ref()
                && matches!(n.method, AcpxMethod::ContextPermissionDecision)
                && let Ok(entry) =
                    serde_json::from_value::<PermissionDecisionEntry>(n.params.clone())
            {
                self.verdict_log_cache.push(entry);
            }
        }
        self.verdict_cursor = self.transcript.frame_count();
        self.todos
            .update(self.transcript.frames(), self.agent_busy());
    }
}
