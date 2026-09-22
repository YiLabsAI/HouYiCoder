//! The transcript domain object: the ordered frame log, the viewable
//! lines derived from it, the current turn boundary, and the revision
//! counter. The frame log lives here so the rebuild path reads frames
//! through this object.

pub(crate) mod blocks;

use std::cell::Cell;
use std::ops::Deref;

use crate::records::TranscriptLine;
use crate::state::CurrentTurnBoundary;
use crate::state::transcript::blocks::TranscriptBlocks;
use crate::transcript::{FrontendRow, SequencedFrame, TranscriptFrame};

#[derive(Debug, Default)]
pub struct Transcript {
    frames: Vec<SequencedFrame>,
    blocks: TranscriptBlocks,
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

    /// Empty the whole transcript state: the viewable lines, the frame log,
    /// and the turn boundary. The revision counter stays monotonic so a
    /// cached render pass never matches a pre-reset version.
    pub(crate) fn reset(&mut self) {
        self.lines.clear();
        self.frames.clear();
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
        dropped
    }
}
