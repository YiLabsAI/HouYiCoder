//! The transcript domain object: the ordered frame log, the viewable
//! lines derived from it, the current turn boundary, and the revision
//! counter. The frame log lives here so the rebuild path reads frames
//! through this object.

pub(crate) mod blocks;

use std::cell::Cell;
use std::ops::Deref;

use crate::fold::{FoldGroup, compute_fold_groups};
use crate::records::TranscriptLine;
use crate::state::CurrentTurnBoundary;
use crate::state::transcript::blocks::TranscriptBlocks;
use crate::transcript::{FrontendRow, SequencedFrame, TranscriptFrame};

#[derive(Debug, Default)]
pub struct Transcript {
    frames: Vec<SequencedFrame>,
    blocks: TranscriptBlocks,
    lines: Vec<TranscriptLine>,
    /// Fold groups over the current lines, maintained incrementally by the
    /// rebuild so every render and count pass reads the cache instead of
    /// rescanning the transcript.
    fold_groups: Vec<FoldGroup>,
    current_turn: CurrentTurnBoundary,
    revision: Cell<u64>,
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

    pub(crate) fn frames_mut(&mut self) -> &mut Vec<SequencedFrame> {
        &mut self.frames
    }

    pub fn push_frame(&mut self, frame: impl Into<SequencedFrame>) {
        self.frames.push(frame.into());
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
        if matches_trailing_echo {
            self.frames.pop();
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

    /// The incrementally maintained fold groups over the current lines.
    pub(crate) fn fold_groups(&self) -> &[FoldGroup] {
        &self.fold_groups
    }

    pub(crate) fn fold_groups_mut(&mut self) -> &mut Vec<FoldGroup> {
        &mut self.fold_groups
    }

    /// Empty the whole transcript state: the viewable lines, the frame log,
    /// and the turn boundary. The revision counter stays monotonic so a
    /// cached render pass never matches a pre-reset version.
    pub(crate) fn reset(&mut self) {
        self.lines.clear();
        self.frames.clear();
        self.fold_groups.clear();
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
    /// this struct holds no scroll state.
    pub(crate) fn trim_live(&mut self, following_tail: bool) -> usize {
        if !following_tail {
            return 0;
        }
        let dropped = crate::scroll::bound_scrollback(&mut self.lines);
        self.current_turn.line_index = self.current_turn.line_index.saturating_sub(dropped);
        if dropped > 0 {
            // Fold groups are absolute-indexed; shift the survivors down and
            // drop those evicted whole. A group straddling the cut keeps its
            // start clamped to the new front (the naively-capped prefix is
            // transient, replaced by the frame window later).
            self.fold_groups.retain(|g| g.end > dropped);
            for g in &mut self.fold_groups {
                g.start = g.start.saturating_sub(dropped);
                g.end -= dropped;
            }
        }
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::ToolOutcome;
    use crate::scroll::VIEWABLE_SCROLLBACK_CAP;

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
}
