//! The transcript domain object: the ordered frame log, the viewable
//! lines derived from it, the current turn boundary, and the revision
//! counter. The frame log lives here so the rebuild path reads frames
//! through this object. Rows read back from the session log for the part of
//! history the frame window dropped live here too, ahead of the lines the
//! blocks derive.

pub(crate) mod blocks;

use std::cell::Cell;
use std::ops::Deref;

use crate::fold::{FoldGroup, compute_fold_groups};
use crate::records::TranscriptLine;
use crate::state::CurrentTurnBoundary;
use crate::state::transcript::blocks::TranscriptBlocks;
use crate::transcript::{FrontendRow, SequencedFrame, TranscriptFrame};

#[derive(Debug)]
pub struct Transcript {
    frames: Vec<SequencedFrame>,
    /// Absolute index of the oldest resident frame. Frames before it were
    /// drained while the resident window was over its byte budget. Block frame
    /// ranges and cursors stay absolute, so block identity survives the drain.
    frame_window_start: usize,
    /// Ceiling on the resident frames' estimated bytes. The rebuild drains the
    /// oldest frames until the resident total falls to it, as far as the frames
    /// it must keep allow: the newest frame always stays, and a turn whose own
    /// bytes exceed the budget holds above it until that turn ends.
    resident_byte_budget: usize,
    /// Estimated bytes of the resident frames, maintained on push and drain.
    resident_bytes: Cell<u64>,
    blocks: TranscriptBlocks,
    lines: Vec<TranscriptLine>,
    /// Fold groups over the current lines, maintained incrementally by the
    /// rebuild so every render and count pass reads the cache instead of
    /// rescanning the transcript.
    fold_groups: Vec<FoldGroup>,
    /// Rows read back from the session log, older than the resident front.
    /// They lead the viewable lines: the rebuild prepends them to the lines
    /// the blocks derive. Empty while the reader follows the tail, whose
    /// capped view cannot reach them.
    disk_rows: Vec<TranscriptLine>,
    /// Where the disk rows begin in the session log, and whether the log holds
    /// anything older than them.
    disk_front: DiskFront,
    /// The resident front the disk rows were read to sit above. A drain moves
    /// that front past them, and the frames between the two fronts are gone,
    /// so the rows no longer reach the view and are dropped.
    disk_rows_front: usize,
    /// A running history read, when the draw path dispatched a disk read to
    /// a background task instead of running it on the draw thread. At most
    /// one: a second dispatch is skipped while this is set. Lives here rather
    /// than on App so the App field count stays bounded and the state sits
    /// beside the disk rows it governs.
    history_read: Option<PendingHistoryRead>,
    current_turn: CurrentTurnBoundary,
    revision: Cell<u64>,
}

/// Where the rows read back from the session log stand, which is what the
/// next older read needs to know: nothing loaded, the byte offset its window
/// ends at, or that no older row was found to show.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum DiskFront {
    /// No rows are loaded. The next read starts at the log tail and drops the
    /// rows the visible transcript already shows.
    #[default]
    Unloaded,
    /// The loaded rows begin at this byte offset, where the next older read
    /// ends.
    At(u64),
    /// No older row was found: the loaded rows reach the log's start, or the
    /// log holds no row the visible front matches.
    Stopped,
}

pub(crate) use super::history_read::{
    HistoryReadOutcome, HistoryReadPoll, HistoryReadResult, PendingHistoryRead,
};

/// Default ceiling on the resident frames' estimated bytes.
const RESIDENT_BYTE_BUDGET: usize = 8 * 1024 * 1024;

impl Default for Transcript {
    fn default() -> Self {
        Self {
            frames: Vec::new(),
            frame_window_start: 0,
            resident_byte_budget: RESIDENT_BYTE_BUDGET,
            resident_bytes: Cell::new(0),
            blocks: TranscriptBlocks::default(),
            lines: Vec::new(),
            fold_groups: Vec::new(),
            disk_rows: Vec::new(),
            disk_front: DiskFront::default(),
            disk_rows_front: 0,
            history_read: None,
            current_turn: CurrentTurnBoundary::default(),
            revision: Cell::new(0),
        }
    }
}

impl Deref for Transcript {
    type Target = [TranscriptLine];

    fn deref(&self) -> &[TranscriptLine] {
        &self.lines
    }
}

impl From<Vec<TranscriptLine>> for Transcript {
    fn from(lines: Vec<TranscriptLine>) -> Self {
        // Built directly from lines, no agent is running, so no group is
        // active; a rebuild re-derives this cache with the real run state.
        let fold_groups = compute_fold_groups(&lines, false);
        Self {
            lines,
            fold_groups,
            ..Default::default()
        }
    }
}

impl Transcript {
    pub(crate) fn frames(&self) -> &[SequencedFrame] {
        &self.frames
    }

    /// Mutate the frame log through a closure so the resident byte counter is
    /// re-derived from the frames afterward: a caller that replaces, pops, or
    /// edits a frame's payload changes the resident total, and a counter that
    /// missed it would mis-size the next eviction. A direct &mut escape cannot
    /// hook the end of the mutation, so the closure is the only door.
    pub(crate) fn with_frames_mut<R>(
        &mut self,
        edit: impl FnOnce(&mut Vec<SequencedFrame>) -> R,
    ) -> R {
        let out = edit(&mut self.frames);
        let total: u64 = self
            .frames
            .iter()
            .map(|sf| sf.estimated_bytes() as u64)
            .sum();
        self.resident_bytes.set(total);
        out
    }

    pub fn push_frame(&mut self, frame: impl Into<SequencedFrame>) {
        let frame = frame.into();
        self.resident_bytes
            .set(self.resident_bytes.get() + frame.estimated_bytes() as u64);
        self.frames.push(frame);
    }

    /// The absolute index of the oldest resident frame. Frames before it were
    /// drained, so a frame's absolute index is its resident position plus this.
    pub(crate) fn frame_window_start(&self) -> usize {
        self.frame_window_start
    }

    /// The absolute frame count: the resident frames plus every frame drained
    /// before them. Block frame ranges and cursors live in this space.
    pub(crate) fn abs_frame_count(&self) -> usize {
        self.frame_window_start + self.frames.len()
    }

    /// The frame at an absolute index, or None when it was drained or never
    /// existed. The resident vec holds [frame_window_start, abs_frame_count).
    pub(crate) fn resident_frame(&self, abs_idx: usize) -> Option<&SequencedFrame> {
        self.frames
            .get(abs_idx.checked_sub(self.frame_window_start)?)
    }

    /// The resident frames' estimated byte total.
    pub(crate) fn resident_bytes(&self) -> u64 {
        self.resident_bytes.get()
    }

    /// The resident frames' byte ceiling.
    pub(crate) fn resident_byte_budget(&self) -> usize {
        self.resident_byte_budget
    }

    #[cfg(test)]
    pub(crate) fn set_resident_byte_budget(&mut self, bytes: usize) {
        self.resident_byte_budget = bytes;
    }

    /// Drain the oldest resident frames until their estimated bytes fall to
    /// the budget. A frame at or after keep_from stays resident; with no
    /// boundary the drain takes any frame but the newest. The drain stops at
    /// the frames it may not take, so a kept suffix that alone exceeds the
    /// budget leaves the resident total above it. Returns how many frames were
    /// dropped. The bytes are summed in the same pass that drops the frames, so
    /// the counter cannot drift from the log.
    pub(crate) fn drain_front_to_budget(&mut self, budget: u64, keep_from: Option<usize>) -> usize {
        let below = match keep_from {
            Some(keep_from) => keep_from
                .saturating_sub(self.frame_window_start)
                .min(self.frames.len()),
            None => self.frames.len(),
        };
        // The newest frame stays resident in every case: a log drained empty
        // has nothing left to render, and one frame above the budget cannot be
        // brought under it.
        let drainable = below.min(self.frames.len().saturating_sub(1));
        let mut freed = 0u64;
        let mut k = 0usize;
        while k < drainable && self.resident_bytes.get().saturating_sub(freed) > budget {
            freed += self.frames[k].estimated_bytes() as u64;
            k += 1;
        }
        if k > 0 {
            self.frames.drain(..k);
            self.frame_window_start += k;
            self.resident_bytes
                .set(self.resident_bytes.get().saturating_sub(freed));
        }
        k
    }

    /// Drop the trailing Echo frame when the server's user message for the same
    /// text is now arriving, so the row renders once from the server frame
    /// instead of twice (the tentative echo the frontend raised plus the
    /// server's own UserMessageChunk). Only the trailing Echo is examined, so
    /// no earlier frame's index shifts and the block ranges that name log
    /// positions stay valid. The caller is the frame batch entry, the one
    /// place that knows a server user message is arriving.
    pub fn drop_trailing_echo(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let matches_trailing_echo = matches!(
            self.frames.last(),
            Some(sf) if matches!(
                sf.as_ref(),
                TranscriptFrame::Frontend(FrontendRow::Echo(t)) if t.as_str() == text
            )
        );
        if matches_trailing_echo && let Some(dropped) = self.frames.pop() {
            self.resident_bytes.set(
                self.resident_bytes
                    .get()
                    .saturating_sub(dropped.estimated_bytes() as u64),
            );
        }
    }

    pub(crate) fn frame_count(&self) -> usize {
        self.frames.len()
    }

    pub(crate) fn blocks(&self) -> &TranscriptBlocks {
        &self.blocks
    }

    pub(crate) fn blocks_mut(&mut self) -> &mut TranscriptBlocks {
        &mut self.blocks
    }

    pub(crate) fn lines(&self) -> &[TranscriptLine] {
        &self.lines
    }

    pub(crate) fn lines_mut(&mut self) -> &mut Vec<TranscriptLine> {
        &mut self.lines
    }

    pub(crate) fn replace_lines(&mut self, lines: Vec<TranscriptLine>) {
        self.lines = lines;
    }

    pub(crate) fn push(&mut self, line: TranscriptLine) {
        self.lines.push(line);
    }

    /// The rows read back from the session log, oldest first.
    pub(crate) fn disk_rows(&self) -> &[TranscriptLine] {
        &self.disk_rows
    }

    pub(crate) fn disk_row_count(&self) -> usize {
        self.disk_rows.len()
    }

    /// Where the next older read starts from.
    pub(crate) fn disk_front(&self) -> DiskFront {
        self.disk_front
    }

    pub(crate) fn set_disk_front(&mut self, front: DiskFront) {
        self.disk_front = front;
    }

    /// The resident front the disk rows sit above.
    pub(crate) fn disk_rows_front(&self) -> usize {
        self.disk_rows_front
    }

    /// Whether a background history read is running.
    pub(crate) fn history_read_pending(&self) -> bool {
        self.history_read.is_some()
    }

    /// Take the pending read out for polling, returning None when none is in
    /// flight. The caller polls and, on Ready or Disconnected, leaves the slot
    /// empty by not putting it back; on Pending it must put it back to keep
    /// the read alive.
    pub(crate) fn take_history_read(&mut self) -> Option<PendingHistoryRead> {
        self.history_read.take()
    }

    /// Put a pending read back after a Pending poll.
    pub(crate) fn set_history_read(&mut self, read: PendingHistoryRead) {
        self.history_read = Some(read);
    }

    /// Put older rows read from the session log in front of the rows the frame
    /// log still holds, anchored at the log byte offset the oldest of them
    /// begins at and at the resident front they sit above. The rows enter the
    /// visible list here rather than at the next rebuild, so the list and the
    /// fold cache that indexes it always agree on where they are. The fold
    /// cache is indexed by line position, so the loaded rows shift every group
    /// down and bring their own groups at the front. Those groups are computed
    /// from the loaded rows alone, so a call whose result is the first resident
    /// row folds without it and a group of a run the frame window drained still
    /// reads as closed.
    pub(crate) fn prepend_disk_rows(
        &mut self,
        rows: Vec<TranscriptLine>,
        anchor: u64,
        front: usize,
    ) {
        if rows.is_empty() {
            return;
        }
        let shift = rows.len();
        for group in &mut self.fold_groups {
            group.start += shift;
            group.end += shift;
        }
        let mut head = compute_fold_groups(&rows, false);
        head.append(&mut self.fold_groups);
        self.fold_groups = head;
        self.current_turn.line_index += shift;
        let mut merged = rows.clone();
        merged.append(&mut self.lines);
        self.lines = merged;
        self.disk_rows.splice(0..0, rows);
        self.disk_front = DiskFront::At(anchor);
        self.disk_rows_front = front;
    }

    /// Drop the rows read back from the session log and the place they stood
    /// at, so the next scroll back reads afresh. The rows leave the visible
    /// list here, in the same step as the fold cache that indexes it: dropping
    /// them from the cache alone would count the same removal twice once the
    /// list itself was cut, shifting every surviving group too far up.
    /// Returns how many rows were dropped.
    pub(crate) fn clear_disk_rows(&mut self) -> usize {
        let dropped = self.disk_rows.len();
        if dropped == 0 {
            return 0;
        }
        self.disk_rows.clear();
        self.disk_front = DiskFront::Unloaded;
        self.disk_rows_front = 0;
        let cut = dropped.min(self.lines.len());
        self.lines.drain(0..cut);
        self.current_turn.line_index = self.current_turn.line_index.saturating_sub(cut);
        self.shift_fold_groups_down(cut);
        dropped
    }

    /// Move every fold group up by a count of lines dropped from the front,
    /// dropping the groups the cut left entirely behind. A group straddling
    /// the cut keeps its surviving tail, its start clamped to the new front.
    fn shift_fold_groups_down(&mut self, dropped: usize) {
        self.fold_groups.retain(|g| g.end > dropped);
        for g in &mut self.fold_groups {
            g.start = g.start.saturating_sub(dropped);
            g.end -= dropped;
        }
    }

    /// The incrementally maintained fold groups over the current lines.
    pub(crate) fn fold_groups(&self) -> &[FoldGroup] {
        &self.fold_groups
    }

    pub(crate) fn fold_groups_mut(&mut self) -> &mut Vec<FoldGroup> {
        &mut self.fold_groups
    }

    /// Empty the whole transcript state: the viewable lines, the frame log,
    /// the rows read back from the session log, and the turn boundary. The
    /// revision counter stays monotonic so a cached render pass never matches
    /// a pre-reset version.
    pub(crate) fn reset(&mut self) {
        self.lines.clear();
        self.frames.clear();
        self.frame_window_start = 0;
        self.resident_bytes.set(0);
        self.fold_groups.clear();
        self.disk_rows.clear();
        self.disk_front = DiskFront::default();
        self.disk_rows_front = 0;
        self.blocks.clear();
        self.current_turn = CurrentTurnBoundary::default();
    }

    pub(crate) fn current_turn_mut(&mut self) -> &mut CurrentTurnBoundary {
        &mut self.current_turn
    }

    pub fn revision(&self) -> u64 {
        self.revision.get()
    }

    pub(crate) fn bump_revision(&self) {
        self.revision.set(self.revision.get().wrapping_add(1));
    }

    /// Cap viewable lines at the tail and shift the turn boundary to match.
    /// No-op while scrolled back: the caller passes the follow-tail flag so
    /// this struct holds no scroll state. Rows read back from the session log
    /// are released first: they belong to the scrolled-back view this call is
    /// ending, and the cap below must not cut them, because their fold groups
    /// are counted against a list they are no longer in.
    pub(crate) fn trim_live(&mut self, following_tail: bool) -> usize {
        if !following_tail {
            return 0;
        }
        let released = self.clear_disk_rows();
        let dropped = crate::scroll::bound_scrollback(&mut self.lines);
        self.current_turn.line_index = self.current_turn.line_index.saturating_sub(dropped);
        if dropped > 0 {
            self.shift_fold_groups_down(dropped);
        }
        released + dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::ToolOutcome;
    use crate::scroll::VIEWABLE_SCROLLBACK_CAP;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};

    fn bash_call(cid: &str) -> TranscriptLine {
        TranscriptLine::Tool {
            name: "bash".into(),
            tool: "bash".into(),
            status: "ls".into(),
            invocation: "ls".into(),
            outcome: ToolOutcome::Success,
            call_id: cid.into(),
            body: String::new(),
            is_diff: false,
        }
    }

    fn bash_result(cid: &str) -> TranscriptLine {
        TranscriptLine::Tool {
            name: "result".into(),
            tool: "bash".into(),
            status: String::new(),
            invocation: String::new(),
            outcome: ToolOutcome::Success,
            call_id: cid.into(),
            body: String::new(),
            is_diff: false,
        }
    }

    /// A front drain past the scrollback cap shifts surviving fold groups down
    /// by the dropped count, so the cache keeps pointing at the rows it
    /// summarizes instead of a stale pre-drain index.
    #[test]
    fn test_trim_shift_fold_group() {
        let mut lines = vec![TranscriptLine::Agent("filler".into()); VIEWABLE_SCROLLBACK_CAP];
        lines.push(TranscriptLine::User("go".into()));
        lines.push(bash_call("c1"));
        lines.push(bash_result("c1"));
        let mut t = Transcript::from(lines);
        let call_idx = VIEWABLE_SCROLLBACK_CAP + 1;
        assert!(
            t.fold_groups().iter().any(|g| g.start == call_idx),
            "bash pair forms a group at the call index"
        );
        let dropped = t.trim_live(true);
        assert_eq!(dropped, 3);
        let groups = t.fold_groups();
        assert_eq!(groups.len(), 1, "the survivors' group is kept");
        assert_eq!(groups[0].start, call_idx - dropped);
    }

    /// A fold group entirely behind the drain is dropped, so no stale index
    /// points into the evicted prefix.
    #[test]
    fn test_trim_drop_fold_group() {
        let mut lines = vec![TranscriptLine::User("go".into())];
        lines.push(bash_call("c1"));
        lines.push(bash_result("c1"));
        lines.extend(
            std::iter::repeat_with(|| TranscriptLine::Agent("filler".into()))
                .take(VIEWABLE_SCROLLBACK_CAP),
        );
        let mut t = Transcript::from(lines);
        assert_eq!(t.fold_groups().len(), 1, "group forms before the drain");
        let dropped = t.trim_live(true);
        assert_eq!(dropped, 3);
        assert!(t.fold_groups().is_empty(), "evicted group is dropped");
    }

    /// A group straddling the drain keeps its surviving tail: when the cut
    /// lands inside the group, the start clamps to the new front instead of
    /// underflowing.
    #[test]
    fn test_trim_clamp_straddled_group() {
        let mut lines = vec![TranscriptLine::User("go".into())];
        lines.push(bash_call("c1"));
        lines.push(bash_result("c1"));
        lines.push(bash_call("c2"));
        lines.push(bash_result("c2"));
        // Two consecutive bash pairs form one group [1, 5); the filler behind
        // them makes the total CAP + 3 so the drain cuts exactly at index 3.
        lines.extend(
            std::iter::repeat_with(|| TranscriptLine::Agent("filler".into()))
                .take(VIEWABLE_SCROLLBACK_CAP - 2),
        );
        let mut t = Transcript::from(lines);
        assert_eq!(t.fold_groups().len(), 1, "one group spans both pairs");
        assert_eq!(t.fold_groups()[0].start, 1);
        assert_eq!(t.fold_groups()[0].end, 5);

        let dropped = t.trim_live(true);
        assert_eq!(dropped, 3);

        let groups = t.fold_groups();
        assert_eq!(groups.len(), 1, "straddled group keeps its surviving tail");
        assert_eq!(groups[0].start, 0, "start clamps to the new front");
        assert_eq!(groups[0].end, 2, "end shifts to the surviving c2 pair");
    }

    fn user_frame(text: &str) -> TranscriptFrame {
        TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
            ContentBlock::Text { text: text.into() },
        )))
    }

    /// The resident byte total follows the frames through a push and a drain,
    /// and the absolute coordinates survive: the drained frames keep their
    /// absolute index while the resident ones stay addressable by it.
    #[test]
    fn test_resident_bytes_track_drain() {
        let mut t = Transcript::default();
        for i in 0..5 {
            t.push_frame(user_frame(&format!("m{i}")));
        }
        let total = t.resident_bytes();
        assert!(total > 0, "a pushed frame counts toward the total");
        assert_eq!(t.abs_frame_count(), 5);

        let freed: u64 = t.frames()[..2]
            .iter()
            .map(|sf| sf.estimated_bytes() as u64)
            .sum();
        // A budget below the total but above what one frame holds drops the
        // oldest frames until the total fits, and stops at the kept front.
        let dropped = t.drain_front_to_budget(total - freed, Some(3));
        assert_eq!(dropped, 2, "the drain takes the frames the budget needs");
        assert_eq!(t.frame_window_start(), 2);
        assert_eq!(t.abs_frame_count(), 5, "draining does not renumber");
        assert_eq!(t.resident_bytes(), total - freed);
        assert!(t.resident_frame(0).is_none(), "a drained frame is gone");
        assert!(t.resident_frame(2).is_some(), "a resident frame stays");
    }

    /// The drain stops at the kept front: a budget far below the total cannot
    /// take the frames the caller protects.
    #[test]
    fn test_drain_keeps_protected() {
        let mut t = Transcript::default();
        for i in 0..5 {
            t.push_frame(user_frame(&format!("m{i}")));
        }
        let dropped = t.drain_front_to_budget(1, Some(3));
        assert_eq!(dropped, 3, "the drain takes every frame below the front");
        assert_eq!(t.frame_window_start(), 3);
        assert_eq!(t.frame_count(), 2, "the kept frames stay resident");
    }

    /// With no boundary the drain takes any frame but the newest, so a log that
    /// holds no frame to protect still reaches its budget. The newest frame
    /// stays even when it alone exceeds the budget: a resident log drained empty
    /// has nothing left to render.
    #[test]
    fn test_drain_without_boundary() {
        let mut t = Transcript::default();
        for i in 0..5 {
            t.push_frame(user_frame(&format!("m{i}")));
        }
        let dropped = t.drain_front_to_budget(1, None);
        assert_eq!(dropped, 4, "the drain takes every frame but the newest");
        assert_eq!(t.frame_count(), 1, "the newest frame stays resident");
        assert_eq!(t.abs_frame_count(), 5, "draining does not renumber");
        assert_eq!(
            t.drain_front_to_budget(1, None),
            0,
            "a lone frame above the budget is not drained away"
        );
    }

    /// Rows read back from the session log lead the viewable lines, and the
    /// fold cache follows them: the groups they bring sit at the front and
    /// every group the resident rows had shifts down by their count. Dropping
    /// the loaded rows shifts the survivors back up.
    #[test]
    fn test_disk_rows_shift_folds() {
        let mut lines = vec![TranscriptLine::User("go".into())];
        lines.push(bash_call("c1"));
        lines.push(bash_result("c1"));
        let mut t = Transcript::from(lines);
        assert_eq!(t.fold_groups()[0].start, 1, "the resident pair folds");

        let older = vec![bash_call("c0"), bash_result("c0")];
        t.prepend_disk_rows(older, 4096, 7);
        assert_eq!(t.disk_row_count(), 2);
        assert_eq!(t.disk_front(), DiskFront::At(4096));
        assert_eq!(t.disk_rows_front(), 7);
        assert_eq!(t.fold_groups().len(), 2, "the loaded pair folds too");
        assert_eq!(t.fold_groups()[0].start, 0, "the loaded group leads");
        assert_eq!(t.fold_groups()[1].start, 3, "the resident group shifted");

        assert_eq!(t.clear_disk_rows(), 2);
        assert_eq!(t.disk_front(), DiskFront::Unloaded);
        assert_eq!(t.disk_row_count(), 0);
        assert_eq!(t.fold_groups().len(), 1);
        assert_eq!(t.fold_groups()[0].start, 1, "the resident group shifts up");
    }

    /// Releasing the rows read back from the session log counts their removal
    /// once. The cap at the tail drops them from the visible list first, and
    /// the fold cache that indexes that list must not count the same removal
    /// again when the rows are released: every surviving group would sit a
    /// loaded-row too high, over lines it does not cover.
    #[test]
    fn test_disk_rows_release_once() {
        let mut lines = vec![
            TranscriptLine::User("go".into()),
            bash_call("c1"),
            bash_result("c1"),
        ];
        while lines.len() <= VIEWABLE_SCROLLBACK_CAP {
            lines.push(TranscriptLine::User("pad".into()));
        }
        let mut t = Transcript::from(lines);
        t.prepend_disk_rows(vec![TranscriptLine::User("older".into())], 4096, 0);

        t.trim_live(true);
        t.clear_disk_rows();

        assert_eq!(t.disk_row_count(), 0);
        let cached: Vec<(usize, usize)> =
            t.fold_groups().iter().map(|g| (g.start, g.end)).collect();
        let fresh: Vec<(usize, usize)> = compute_fold_groups(t.lines(), false)
            .iter()
            .map(|g| (g.start, g.end))
            .collect();
        assert_eq!(
            cached, fresh,
            "the fold cache indexes the list the release left"
        );
    }

    /// A payload edit through the closure door re-derives the byte total, so a
    /// frame that grew cannot leave the counter under the true size and let a
    /// later eviction hold more than the budget.
    #[test]
    fn test_frames_mut_recounts_bytes() {
        let mut t = Transcript::default();
        t.push_frame(user_frame("short"));
        let before = t.resident_bytes();
        t.with_frames_mut(|log| {
            *log = vec![user_frame(&"x".repeat(4096)).into()];
        });
        assert!(
            t.resident_bytes() > before + 4000,
            "the swapped frame's larger payload counts"
        );
    }
}
