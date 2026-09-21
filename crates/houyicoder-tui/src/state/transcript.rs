//! The transcript domain object: viewable lines, the current turn
//! boundary, and the revision counter. The frame log stays on App until
//! the run-state refactor closes and the rebuild path can be reshaped to
//! read frames through this object without crossing the run lifecycle.

use std::cell::Cell;
use std::ops::Deref;

use crate::records::TranscriptLine;
use crate::state::CurrentTurnBoundary;

#[derive(Debug, Default)]
pub struct Transcript {
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

    pub(crate) fn clear(&mut self) {
        self.lines.clear();
    }

    pub(crate) fn current_turn(&self) -> &CurrentTurnBoundary {
        &self.current_turn
    }

    pub(crate) fn current_turn_mut(&mut self) -> &mut CurrentTurnBoundary {
        &mut self.current_turn
    }

    pub(crate) fn reset_current_turn(&mut self) {
        self.current_turn = CurrentTurnBoundary::default();
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
