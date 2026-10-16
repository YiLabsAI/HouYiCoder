//! The row list the view renders, each row beside the identity it is named
//! by. The names are what a scroll anchor, a fold key, a search jump, and an
//! expand toggle resolve through, so the two lists are written together by the
//! operations here rather than exposed as two fields a caller keeps in step.

use crate::records::TranscriptLine;
use crate::row_id::RowId;

/// Rows with the identity each is named by. Every change to the two lists goes
/// through the operations here, so a caller cannot add, drop, or replace a row
/// without the identity that stands at the same position.
#[derive(Debug, Default, Clone)]
pub struct NamedRows {
    lines: Vec<TranscriptLine>,
    ids: Vec<RowId>,
}

impl NamedRows {
    /// Rows named by position, for a list whose entries carry no identities.
    /// Every name is local, so a match against a durable entry falls back to
    /// text — the behavior a log without entry identities already had.
    pub fn from_lines(lines: Vec<TranscriptLine>) -> Self {
        let ids = (0..lines.len() as u64).map(RowId::local).collect();
        Self { lines, ids }
    }

    /// Rows a walk already holds beside the identity of the entry each came
    /// from, in the order the walk produced them. Both are checked at this
    /// door rather than built here: the caller owns the two lists, and a line
    /// left without its identity has nothing that names it.
    pub(crate) fn from_parts(lines: Vec<TranscriptLine>, ids: Vec<RowId>) -> Self {
        debug_assert_eq!(lines.len(), ids.len(), "every row carries its identity");
        Self { lines, ids }
    }

    /// The rows.
    pub fn lines(&self) -> &[TranscriptLine] {
        &self.lines
    }

    /// The identity of each row, at the same position.
    pub fn ids(&self) -> &[RowId] {
        &self.ids
    }

    /// The rows, for a pass that rewrites a row's own payload. A slice rather
    /// than the list itself: rewriting what a row holds leaves every position
    /// and every identity where it was, while adding or dropping a row does
    /// not, and those go through the operations below.
    pub(crate) fn lines_mut(&mut self) -> &mut [TranscriptLine] {
        &mut self.lines
    }

    /// The rows alone, for a caller that renders them without naming any.
    pub fn into_lines(self) -> Vec<TranscriptLine> {
        self.lines
    }

    /// How many rows the list holds.
    pub(crate) fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether the list holds no rows.
    pub(crate) fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The row at this position, for a read that changes nothing.
    pub(crate) fn line(&self, index: usize) -> Option<&TranscriptLine> {
        self.lines.get(index)
    }

    /// The row at this position, for a write that changes its payload and
    /// nothing else. No identity moves, so no other row is renumbered.
    pub(crate) fn line_mut(&mut self, index: usize) -> Option<&mut TranscriptLine> {
        self.lines.get_mut(index)
    }

    /// Append one row under the identity that names it.
    pub(crate) fn push(&mut self, line: TranscriptLine, id: RowId) {
        self.lines.push(line);
        self.ids.push(id);
    }

    /// Append another list's rows, each beside its own identity.
    pub(crate) fn append(&mut self, other: &NamedRows) {
        self.lines.extend_from_slice(&other.lines);
        self.ids.extend_from_slice(&other.ids);
    }

    /// Keep the first n rows, lines and identities together.
    pub(crate) fn truncate(&mut self, n: usize) {
        self.lines.truncate(n);
        self.ids.truncate(n);
    }

    /// Put these rows in front of the current ones, keeping every line beside
    /// its own identity.
    pub(crate) fn prepend(&mut self, front: NamedRows) {
        if front.lines.is_empty() {
            return;
        }
        self.lines.splice(0..0, front.lines);
        self.ids.splice(0..0, front.ids);
    }

    /// Drop the first n rows, lines and identities together.
    pub(crate) fn drop_front(&mut self, n: usize) {
        let n = n.min(self.lines.len());
        self.lines.drain(0..n);
        self.ids.drain(0..n);
    }

    /// Drop every row.
    pub(crate) fn clear(&mut self) {
        self.lines.clear();
        self.ids.clear();
    }
}
