//! The transcript domain object: the ordered frame log, the viewable
//! lines derived from it, the current turn boundary, and the revision
//! counter. The frame log lives here so the rebuild path reads frames
//! through this object.

use std::cell::Cell;
use std::ops::Deref;

use crate::records::TranscriptLine;
use crate::state::CurrentTurnBoundary;
use crate::transcript::TranscriptFrame;

#[derive(Debug, Default)]
pub struct Transcript {
    frames: Vec<TranscriptFrame>,
    lines: Vec<TranscriptLine>,
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
        Self {
            lines,
            ..Default::default()
        }
    }
}

impl Transcript {
    pub(crate) fn frames(&self) -> &[TranscriptFrame] {
        &self.frames
    }

    pub(crate) fn frames_mut(&mut self) -> &mut Vec<TranscriptFrame> {
        &mut self.frames
    }

    pub fn push_frame(&mut self, frame: TranscriptFrame) {
        self.frames.push(frame);
    }

    pub(crate) fn frame_count(&self) -> usize {
        self.frames.len()
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

    /// Empty the whole transcript state: the viewable lines, the frame log,
    /// and the turn boundary. The revision counter stays monotonic so a
    /// cached render pass never matches a pre-reset version.
    pub(crate) fn reset(&mut self) {
        self.lines.clear();
        self.frames.clear();
        self.current_turn = CurrentTurnBoundary::default();
    }

    pub(crate) fn current_turn(&self) -> &CurrentTurnBoundary {
        &self.current_turn
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
        dropped
    }
}
