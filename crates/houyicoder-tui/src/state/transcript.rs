//! The transcript domain object: the ordered frame log, the viewable
//! lines derived from it, the current turn boundary, and the revision
//! counter. The frame log lives here so the rebuild path reads frames
//! through this object. Rows read back from the session log for the part of
//! history the frame window dropped live here too, ahead of the lines the
//! blocks derive.

pub(crate) mod blocks;
pub(crate) mod view;

use std::cell::Cell;

use crate::fold::{FoldGroup, compute_fold_groups};
use crate::records::{ContextView, TranscriptLine};
use crate::state::transcript::blocks::TranscriptBlocks;
use crate::state::{App, CurrentTurnBoundary};
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
            current_turn: CurrentTurnBoundary::default(),
            revision: Cell::new(0),
        }
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

    /// The resident lines, oldest first. The frames, the byte budget, and the
    /// fold groups are the transcript's other halves, so a reader asks for the
    /// conversation by name instead of dereferencing to it.
    pub fn lines(&self) -> &[TranscriptLine] {
        &self.lines
    }

    /// How many lines the resident window holds. Not the frame count: frames
    /// carry the log's own boundaries, lines are what the view renders.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, TranscriptLine> {
        self.lines.iter()
    }

    pub fn first(&self) -> Option<&TranscriptLine> {
        self.lines.first()
    }

    pub fn last(&self) -> Option<&TranscriptLine> {
        self.lines.last()
    }

    pub fn get(&self, index: usize) -> Option<&TranscriptLine> {
        self.lines.get(index)
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

/// App's methods over the transcript: how lines and frontend-raised rows
/// enter the live transcript and how the content version is bumped. Lives
/// with the Transcript owner rather than in an App method grab-bag, so the
/// call sites that mutate it sit beside the data they reach.
impl App {
    /// Push a system line onto the transcript. The scroll state is left
    /// untouched (see push_transcript_line); a user following the tail stays
    /// on the tail via top_offset, a user scrolled back stays scrolled back.
    pub fn system_line(&mut self, msg: impl Into<String>) {
        self.push_transcript_line(TranscriptLine::System(msg.into()));
    }
    /// Push a transcript line without touching the scroll state. A user
    /// following the tail stays on the tail via top_offset on the next draw;
    /// a user scrolled back stays in place — agent output never yanks a
    /// reader of history. User-initiated re-follow (input, End, Ctrl+End,
    /// PageDown to bottom) happens at the action site, not here.
    ///
    /// A line the frontend raises on its own account (a system line, a context
    /// view, an interrupt notice) enters the frame log at the position it
    /// happened and renders from there: the log, not this list, is what holds
    /// its place.
    pub fn push_transcript_line(&mut self, line: TranscriptLine) {
        match FrontendRow::from_line(&line) {
            Some(row) => self.raise_frontend_row(row),
            None => {
                self.sessions.current.transcript.push(line);
                self.trim_live_transcript();
                self.bump_transcript_version();
            }
        }
    }
    /// Show input the user submitted that no server frame will carry: a command,
    /// which never reaches the model, or a message in an app with no runner
    /// connected. The row enters the frame log, which keeps its place for a
    /// rebuild and drops it with a window that no longer covers its frame.
    pub fn push_unanswered_echo(&mut self, text: String) {
        self.raise_frontend_row(FrontendRow::Echo(text));
    }
    /// Append a row the frontend raises to the frame log, then rebuild so the
    /// row renders where its frame now sits. Every such append ends here: a
    /// frame the fold has not seen is a row the transcript is missing.
    pub(crate) fn raise_frontend_row(&mut self, row: FrontendRow) {
        self.sessions
            .current
            .transcript
            .push_frame(TranscriptFrame::Frontend(row));
        self.rebuild_transcript();
    }
    /// Drop the tentative command echo raised for an input that turned out
    /// not to be a command, so the message path below raises it once as a
    /// prompt.
    pub fn drop_tentative_echo(&mut self) {
        if matches!(
            self.sessions.current.transcript.frames().last(),
            Some(sf) if matches!(sf.as_ref(), TranscriptFrame::Frontend(FrontendRow::Echo(_)))
        ) {
            self.sessions
                .current
                .transcript
                .with_frames_mut(|log| log.pop());
            self.rebuild_transcript();
        }
    }
    /// Replace the newest context view with a fresh one, or raise it when the
    /// log holds none yet. The row keeps the place its frame holds, so a
    /// refresh updates the grid where it stands instead of stacking another.
    pub fn replace_context_view(&mut self, view: ContextView) {
        let at = self
            .sessions
            .current
            .transcript
            .frames()
            .iter()
            .rposition(|sf| {
                matches!(
                    sf.as_ref(),
                    TranscriptFrame::Frontend(FrontendRow::Context(_))
                )
            });
        match at {
            Some(at) => {
                self.sessions.current.transcript.with_frames_mut(|log| {
                    if let TranscriptFrame::Frontend(FrontendRow::Context(slot)) =
                        &mut log[at].frame
                    {
                        *slot = view;
                    }
                });
                self.rebuild_after_frame_edit();
            }
            None => self.raise_frontend_row(FrontendRow::Context(view)),
        }
    }
    /// Invalidate the render pass's cached row set by bumping the transcript
    /// content version. Every mutation of transcript content must end here,
    /// including one that swaps a payload into an existing line rather than
    /// pushing a new one: the row cache is keyed on this version, so a
    /// mutation that skips the bump renders the pre-mutation rows until some
    /// unrelated change happens to bump it — the change appears to be lost,
    /// then materializes later at the wrong moment.
    pub fn bump_transcript_version(&self) {
        self.sessions.current.transcript.bump_revision();
    }
    /// Cap the live transcript to the viewable scrollback and shift the turn
    /// boundary to match. Returns the number of lines dropped. A no-op while
    /// scrolled back: the viewport holds a fixed top offset, so draining the
    /// oldest lines would shift the content under the reader, and a scroll-up
    /// session's loaded older frames would be drained on the next rebuild.
    /// Trim runs only at the tail, where the offset does not name a viewport
    /// position. The caller bumps the version when this returns a nonzero
    /// drop; the rebuild path bumps once for the whole transcript change
    /// regardless.
    pub(crate) fn trim_live_transcript(&mut self) -> usize {
        self.sessions
            .current
            .transcript
            .trim_live(self.whole_scroll().is_following_tail())
    }
}

#[cfg(test)]
mod tests;
