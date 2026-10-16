//! Incremental transcript rebuilding from the ordered frame log.
//!
//! The visible history is bounded, and the stable prefix is reused while new
//! frames extend the current turn. Rewind and scrollback loading invalidate only
//! the affected range, keeping rebuild cost independent of session length.

use std::ops::Range;

use super::{MAX_REBUILD_FRAMES, PREPEND_BATCH};
use crate::fold::last_turn_boundary;
use crate::fold::{compute_fold_groups, fold_groups_in};
use crate::records::{TURN_CONTENT_OUT_OF_VIEW, TranscriptLine};
use crate::row_id::RowId;
use crate::scroll::VIEWABLE_SCROLLBACK_CAP;
use crate::state::App;
use crate::state::transcript::blocks::{
    Block, BlockAnchor, BlockId, TranscriptChange, TranscriptChangeSet,
};
use crate::transcript::TranscriptFrame;
use crate::transcript::rows::NamedRows;
use crate::transcript::{
    TurnCheckpoint, delivered_marker_at, starts_inside_turn, transcript_from_frames_at,
    turn_state_from_frames_at,
};
use houyicoder_protocol::acpx::AcpxMethod;
use houyicoder_protocol::frontend::permission::PermissionDecisionEntry;
use houyicoder_protocol::frontend::session_update::SessionUpdate;
use std::collections::HashMap;
use std::collections::HashSet;
use std::mem::take;

/// Rows the view holds from the session log before it stops reading older
/// ones: the same bound the viewable transcript keeps. The bound is checked
/// before a read rather than after, so one read can pass it, and by however
/// many rows that read's own byte budget holds, which no row count limits.
const LOG_ROW_BUDGET: usize = VIEWABLE_SCROLLBACK_CAP;

impl App {
    /// Rebuild the transcript from the frame log. Each turn is one block whose
    /// identity (the log position of the frame that opened it) survives
    /// rebuild, so a turn whose frames did not change carries its derived
    /// lines and expand state forward without re-deriving them or re-pairing
    /// them by content. Only the turns whose frame range changed re-derive; a
    /// rewind truncates the stale tail.
    pub(crate) fn rebuild_transcript(&mut self) {
        let turn_start = self.current_turn_start();
        let newest_open = self.sessions.current.run.is_active();
        let frame_start = self.visible_frame_start();
        let frame_end = self.sessions.current.transcript.abs_frame_count();
        // Rows read back from the session log sit above the capped view while
        // the reader is at the tail, which is the only view that cannot reach
        // them. They leave with it, so the next scroll back reads afresh. They
        // also leave when the resident front moved past the rows they were
        // read to sit above: the frames between the two fronts are drained, so
        // holding them would leave a hole in the scrollback.
        if self.whole_scroll().is_following_tail()
            || self.transcript().disk_rows_front() != self.transcript().frame_window_start()
        {
            self.sessions.current.transcript.clear_disk_rows();
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
            self.sessions.current.transcript.blocks_mut().clear();
        }
        self.sessions.current.transcript.blocks_mut().apply(changes);

        // The rows the view shows, each beside its identity, and where the
        // current turn starts among them. The drain below runs after this, so
        // the rows it reclaims still render in the pass that drops them.
        let (rows, prefix_line_count) =
            self.sessions
                .current
                .transcript
                .visible_rows(frame_start, frame_end, turn_start);
        self.sessions.current.transcript.replace_rows(rows);
        let transcript = &mut self.sessions.current.transcript;
        transcript.current_turn_mut().line_index = prefix_line_count;
        self.update_fold_cache(prefix_line_count, prefix_reused);

        // Every resident frame is scanned before the drain drops any of them,
        // so a drained frame's verdicts are already in the audit cache.
        self.accumulate_wire_state();
        self.enforce_frame_byte_budget();
        // A drain in this pass moved the resident front past the frame the
        // loaded rows were cut at, so the rows leave in the same pass: the
        // frames between the two fronts are gone, and rows above a front that
        // does not continue them would render a gap.
        if self.transcript().disk_row_count() > 0
            && self.transcript().disk_rows_front() != self.transcript().frame_window_start()
        {
            self.sessions.current.transcript.clear_disk_rows();
        }
        self.trim_live_transcript();
        self.bump_transcript_version();
        // Resolve the parked viewport against the rows just assembled. Rows
        // entering or leaving this pass moved the row the viewport stands on,
        // and the anchor names that row rather than a number the pass would
        // have to patch up by how many rows it added or released.
        self.settle_whole_scroll();
    }

    /// The identity of the frame at an absolute log position: the identity the
    /// frame carries when the read gives it one, the position itself otherwise.
    /// A turn's block anchors on the frame that opened it, so the block is
    /// reused on the same name its rows were derived under.
    fn frame_row(&self, abs: usize, base: usize) -> RowId {
        match self.transcript().frames().get(abs.saturating_sub(base)) {
            Some(sf) => RowId::of(sf.as_ref(), sf.event_id.as_ref(), abs as u64),
            None => RowId::local(abs as u64),
        }
    }

    /// The state the window's first range opens inside, and the row standing in
    /// for the part of its turn the window cannot show. A turn already open at
    /// the range's first frame was opened by a frame below it, so the frames
    /// between them are ones that pass does not derive, and the fold continues
    /// the turn instead of starting it there. Both come from the state the
    /// drain carried, so a window the drain has taken no frame from reads its
    /// own frames and stands in for nothing: every frame below its front is
    /// still in memory.
    fn window_entry(
        &self,
        ranges: &[Range<usize>],
        base: usize,
    ) -> (Option<TurnCheckpoint>, Option<RowId>) {
        let entry = ranges
            .first()
            .and_then(|range| self.turn_state_at(range.start));
        let cut_row = entry
            .as_ref()
            .and_then(|state| self.cut_row(state, ranges, base));
        (entry, cut_row)
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
        let base = self.transcript().frame_window_start();
        let blocks = self.sessions.current.transcript.blocks().blocks();
        // Evict blocks the cap advanced past: a block whose frame range ended
        // before the window start is out of view, so name the first survivor
        // for EvictBefore to drain the aged-out prefix. When no block
        // survives the advance, the whole list is cleared before re-derive.
        while cur_idx < blocks.len() && blocks[cur_idx].frame_range.end <= frame_start {
            cur_idx += 1;
        }
        if cur_idx > 0 {
            if let Some(survivor) = blocks.get(cur_idx) {
                changes.push(TranscriptChange::EvictBefore {
                    id: survivor.id.clone(),
                });
            } else {
                clear_all = true;
            }
        }
        let (entry, cut_row) = self.window_entry(ranges, base);
        // The state the range before the current one ended a turn with, which
        // the current range's fold continues from. The ranges run in log order
        // and meet at their boundary, so the active turn carries the prefix's
        // state rather than folding the prefix again to find it.
        let mut carried: Option<TurnCheckpoint> = entry;
        for (ri, range) in ranges.iter().enumerate() {
            let is_active = has_active && ri + 1 == ranges.len();
            // The frozen prefix is a single reusable slot whose identity
            // stays fixed across cap advances (the window slides under it),
            // so it anchors on a name of its own rather than the moving
            // frame_start; the active turn anchors on the identity its opening
            // frame carries — the durable entry when the frame came from the
            // log, the frame's log position when the frontend raised it. The
            // stream seq never anchors a block: numbering restarts with the
            // service process, so an identity built from it would drift.
            let id = if is_active {
                let anchor = self.frame_row(range.start, base);
                self.sessions.current.transcript.blocks().assign_id(anchor)
            } else {
                BlockId(BlockAnchor::Frozen)
            };
            let slot = blocks.get(cur_idx);
            let matched = slot.is_some_and(|b| b.id == id);
            // The window's first range marks where its content was cut: a
            // range starting inside a turn whose opening frame the window no
            // longer holds derives rows that begin mid-turn, and the row says
            // so. The later ranges start where a turn opens, so the row is
            // theirs to say nothing about. The count reaches the block, since
            // whoever reuses or reads those rows has to know the first of them
            // is not a row the log holds.
            let stand_in = usize::from(ri == 0 && cut_row.is_some());
            // A range's rows come from its own frames and from the frame that
            // follows its last one, which decides how that message reads, so
            // reuse needs the id, the range, and that frame to be the ones the
            // block was derived from. The frame after the range is the one that
            // moves: a mark for the range's trailing message arrives after the
            // block was derived, and the rows under the old decision — a
            // summary row for a turn the message never opened — must not stay.
            // A frame replaced in place is the tentative echo the user's own
            // message supersedes, which is always the trailing frame, so the
            // boundary that cuts the ranges moves with it; a refresh under a
            // frame's own position replaces no block, it clears them all.
            let marker = range
                .end
                .checked_sub(1)
                .and_then(|at| at.checked_sub(base))
                .and_then(|at| delivered_marker_at(self.transcript().frames(), at))
                .map(|at| self.frame_row(base + at, base));
            let unchanged = slot.is_some_and(|b| {
                b.frame_range == *range
                    && b.stand_in_rows == stand_in
                    && b.checkpoint.delivered_marker() == marker.as_ref()
            });
            if matched && !is_active && unchanged {
                // Unchanged frozen prefix: keep its lines and fold groups
                // as they are, so only the tail re-derives. Its frames did not
                // change, so the state they end a turn with did not either.
                prefix_reused = true;
                carried = slot.map(|b| b.checkpoint.clone());
                cur_idx += 1;
                continue;
            }
            let (mut rows, checkpoint) = transcript_from_frames_at(
                self.sessions.current.transcript.frames(),
                base,
                range.clone(),
                newest_open && is_active,
                carried.take(),
            );
            // The window's first range marks where its content was cut: a
            // range starting inside a turn whose opening frame the window no
            // longer holds derives rows that begin mid-turn, and the row says
            // so. The later ranges start where a turn opens, so the row is
            // theirs to say nothing about.
            if stand_in > 0
                && let Some(opening) = &cut_row
            {
                rows.prepend(NamedRows::from_parts(
                    vec![TranscriptLine::System(TURN_CONTENT_OUT_OF_VIEW.to_string())],
                    vec![opening.clone()],
                ));
            }
            let revision = slot.map(|b| b.revision.wrapping_add(1)).unwrap_or(0);
            carried = Some(checkpoint.clone());
            let block = Block {
                id,
                frame_range: range.clone(),
                rows,
                stand_in_rows: stand_in,
                revision,
                checkpoint,
            };
            match slot {
                // Slot holds a block whose identity no longer fits the
                // range (the cap slid the frozen window, or a new turn
                // took the active slot): replace it in place by its old
                // identity so the slot order is preserved without a rewind.
                Some(old) if !matched => {
                    changes.push(TranscriptChange::ReplaceBlock {
                        id: old.id.clone(),
                        block,
                    });
                    cur_idx += 1;
                }
                Some(_) => {
                    let id = block.id.clone();
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
            changes.push(TranscriptChange::RewindTo {
                id: last_kept.id.clone(),
            });
        }
        (changes, clear_all, prefix_reused)
    }

    /// Recompute the fold cache after a rebuild. When the frozen prefix was
    /// reused its lines and groups are unchanged, so only the active tail is
    /// rescanned; otherwise a turn grew or the window slid and everything is
    /// recomputed. The cache is keyed by absolute line index, so the split
    /// point is the prefix's line count.
    fn update_fold_cache(&mut self, prefix_line_count: usize, prefix_reused: bool) {
        let agent_busy = self.agent_busy();
        let groups = if prefix_reused {
            let mut groups = take(self.sessions.current.transcript.fold_groups_mut());
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
                &self.sessions.current.transcript.lines()[prefix_line_count..],
                prefix_line_count,
                agent_busy,
                &mut ordinal,
            ));
            groups
        } else {
            compute_fold_groups(self.sessions.current.transcript.lines(), agent_busy)
        };
        *self.sessions.current.transcript.fold_groups_mut() = groups;
    }

    /// Recompute each group's active flag after the run pauses or resumes.
    /// The flag bakes in the agent-busy state at the last rebuild, so a
    /// Waiting transition (approval card up, no new frames) would leave it
    /// stale until the next rebuild. Rare, so the full boundary scan is fine.
    pub(crate) fn refresh_fold_active(&mut self) {
        let agent_busy = self.agent_busy();
        let threshold = last_turn_boundary(self.sessions.current.transcript.lines());
        for g in self.sessions.current.transcript.fold_groups_mut() {
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
        self.sessions.current.transcript.blocks_mut().clear();
        self.rebuild_transcript();
    }

    /// The oldest frame the frame cap keeps in view, in absolute frame
    /// coordinates. A scrollback load may reach further back than this.
    fn capped_frame_start(&self) -> usize {
        self.transcript()
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
            .min(self.sessions.current.loaded_from_frame.get())
            .max(self.sessions.current.transcript.frame_window_start())
    }

    /// Drain the oldest resident frames until their estimated bytes fall to
    /// the budget, the frames of the active turn included: a turn whose own
    /// bytes exceed the budget would otherwise hold the whole log resident
    /// until it ends. Frames below the front leave the viewable window, where
    /// the row the window opens with marks their absence; the durable log
    /// still holds the server frames, while rows the frontend raised live only
    /// in this log.
    fn enforce_frame_byte_budget(&mut self) {
        let budget = self.sessions.current.transcript.resident_byte_budget() as u64;
        if self.sessions.current.transcript.resident_bytes() <= budget {
            return;
        }
        self.sessions
            .current
            .transcript
            .drain_front_to_budget(budget);
    }

    /// The state the turn reached by the absolute frame position at, for a
    /// read of a window the drain moved past the frames below it. The state the
    /// drain carried is where the read starts from, advanced over the resident
    /// frames the window leaves out.
    ///
    /// A log the drain has taken no frame from holds every frame below the
    /// read, so no state is handed over: the fold reads the turn's beginning
    /// from the frames below it rather than folding the whole prefix to reach
    /// the same answer, which would run on every rebuild of a long session.
    fn turn_state_at(&self, at: usize) -> Option<TurnCheckpoint> {
        let transcript = &self.sessions.current.transcript;
        let carried = transcript.front_checkpoint().cloned()?;
        turn_state_from_frames_at(
            transcript.frames(),
            transcript.frame_window_start(),
            Some(carried),
            at,
        )
    }

    /// The identity of the row standing in for the part of its turn the window
    /// cannot show, for a window that opens inside a turn. The frame at the
    /// window front decides it: a turn state reads open whenever the frames
    /// below the window left a turn running, which includes a window starting
    /// on the message that opens the next turn — that message ends the turn
    /// before it, and the window begins where that turn did. A window whose
    /// first frame carries no turn on holds every row of the turn it shows, so
    /// no row stands in for anything.
    ///
    /// The state is the one the drain carried, so the row is drawn for a part
    /// of a turn that left memory. The rows the frame cap alone keeps out of
    /// the window are still in memory and come back by scrolling, which the
    /// pane's own hints cover.
    ///
    /// Rows read back from the log sit above the window, and the part of the
    /// turn they carry is on view: a row saying that part left memory would be
    /// false, so none is drawn while they are there.
    fn cut_row(
        &self,
        state: &TurnCheckpoint,
        ranges: &[Range<usize>],
        base: usize,
    ) -> Option<RowId> {
        if self.transcript().disk_row_count() > 0 {
            return None;
        }
        let opens_inside_turn = ranges
            .first()
            .and_then(|range| range.start.checked_sub(base))
            .is_some_and(|at| starts_inside_turn(self.transcript().frames(), at));
        if !(state.is_open() && opens_inside_turn) {
            return None;
        }
        state.opened_at().cloned()
    }

    /// Load older history when scrollback reaches the current boundary,
    /// preserving the visible viewport position. The boundary is the oldest
    /// frame the resident window holds: below it the frames were drained, and
    /// only the session log still carries the rows they folded. The window
    /// now starts further back, so the block list is re-derived for the
    /// enlarged range.
    pub(crate) fn load_older_frames(&mut self) {
        // The parent's history loads only while the parent is on view. A child
        // view hides the parent, and loading hidden history would spend disk
        // reads and mutate parent scroll state the reader cannot see; the gate
        // lives here, not at the call site, so a later caller cannot bypass it.
        if self.ui.teammate_view.is_some() {
            return;
        }
        // Don't prepend when following the tail (user is at the bottom).
        if self.whole_scroll().is_following_tail() {
            return;
        }
        let top = self.whole_scroll().top_offset();
        // Load only when the user is near the current history boundary.
        if top > 5 {
            return;
        }
        let from = self.visible_frame_start();
        if from <= self.sessions.current.transcript.frame_window_start() {
            // The frames below the resident front are gone: dispatch a
            // background read of their rows from the session log. The draw
            // path never waits on disk; the result lands on a later pump.
            self.dispatch_history_read(None);
            return;
        }
        // A batch reaching past the resident front can only load the frames that
        // are still present: the rest were drained, and their rows come from the
        // session log. The boundary stops at the front so the window it names
        // stays derivable.
        let batch_start = from
            .saturating_sub(PREPEND_BATCH)
            .max(self.sessions.current.transcript.frame_window_start());
        // The batch's frames all precede the frames already loaded, and a row
        // the frontend raised among them rides the batch's own fold. A batch
        // that yields no lines lowered the boundary above all the same, so the
        // walk reads further back next time; there are no rows to rebuild.
        // The batch enters on the state the turn reached by its first frame:
        // a batch that reaches the resident front opens inside the turn the
        // drain carried past its frames, so the rows it derives belong to that
        // turn rather than starting one the reader's view would have to invent.
        let entry = self.turn_state_at(batch_start);
        let prepended = transcript_from_frames_at(
            self.sessions.current.transcript.frames(),
            self.sessions.current.transcript.frame_window_start(),
            batch_start..from,
            self.sessions.current.run.is_active(),
            entry,
        )
        .0
        .len();
        self.sessions.current.loaded_from_frame.set(batch_start);
        if prepended == 0 {
            return;
        }
        // The lowered boundary moves the window's front, so the next rebuild
        // re-derives the prefix rather than reusing it. The rebuild resolves
        // the parked viewport afterwards, so the rows this batch adds above it
        // move the viewport with their content.
        self.rebuild_transcript();
        self.bump_transcript_version();
    }

    /// Return the first frame in the changing turn. If that boundary would
    /// split a tool call from its result, move it backward until the pair stays
    /// together. The oldest boundary this can name is the resident front: the
    /// frames before it are drained, so a turn whose opening frame they held
    /// is re-derived from the front on the next rebuild.
    pub(crate) fn current_turn_start(&self) -> usize {
        let base = self.sessions.current.transcript.frame_window_start();
        let mut search_from = self.sessions.current.transcript.frames().len();
        loop {
            let Some(idx) = self.sessions.current.transcript.frames()[..search_from]
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
        let base = self.sessions.current.transcript.frame_window_start();
        let frames = self.sessions.current.transcript.frames();
        let at = candidate.saturating_sub(base).min(frames.len());
        let mut calls = HashSet::new();
        let mut results = HashSet::new();
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
        let mut tail_results = HashSet::new();
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
        // Verdicts are append-only (audit trail). Rewind and clear truncate
        // frames below the cursor, so the cache resets and re-parses from zero
        // to match the truncated log. A front drain leaves the absolute count
        // unchanged, so it never trips this, and the rebuild drains only after
        // this scan, so every drained frame's verdict is already cached.
        let permission = &mut self.sessions.current.capabilities.permission;
        if permission.verdict_cursor > self.sessions.current.transcript.abs_frame_count() {
            permission.verdict_cursor = 0;
            permission.verdict_log_cache.clear();
        }
        let base = self.sessions.current.transcript.frame_window_start();
        let frames = self.sessions.current.transcript.frames();
        for sf in frames
            .iter()
            .skip(permission.verdict_cursor.saturating_sub(base))
        {
            if let TranscriptFrame::Acpx(n) = sf.as_ref()
                && matches!(n.method, AcpxMethod::ContextPermissionDecision)
                && let Ok(entry) =
                    serde_json::from_value::<PermissionDecisionEntry>(n.params.clone())
            {
                permission.verdict_log_cache.push(entry);
            }
        }
        permission.verdict_cursor = self.sessions.current.transcript.abs_frame_count();
        self.sessions.current.todos.update(
            self.sessions.current.transcript.frames(),
            self.sessions.current.transcript.frame_window_start(),
            self.agent_busy(),
        );
    }
}

mod read_pump;

#[cfg(test)]
mod tests;
