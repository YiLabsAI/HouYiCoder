//! What the index keeps for one result: its listing row, the block its bytes
//! live in, and how those bytes are to be read.

use std::sync::{Arc, Mutex};

use houyicoder_context::BlockHash;
use houyicoder_protocol::result_catalog::ResultEntry;

/// How a stored block spells the bytes it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StoredShape {
    /// The stream exactly as the producer wrote it.
    Raw,
    /// One JSON string value, quotes and escapes included.
    JsonString,
    /// JSON text that is not a single string: a serialized field or the
    /// serialized whole output.
    JsonText,
}

/// Where one result's bytes live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BlockSource {
    /// The content address of the block.
    pub hash: BlockHash,
    /// How the stored bytes encode the stream.
    pub shape: StoredShape,
}

/// The line marks a line read leaves behind, so a second read of the same
/// result does not scan from its beginning.
///
/// Only every mark stride line is kept. Between two marks the scan is still
/// linear, which bounds the marks one very long result can hold.
#[derive(Debug, Default)]
pub(super) struct LineMarks {
    /// Stored offset of the first byte of line k times the stride, in line
    /// order.
    marks: Vec<u64>,
}

/// How many lines one mark covers.
pub(super) const MARK_STRIDE: usize = 256;

/// How many marks one result keeps. Past this the scan starts from the last
/// mark it holds rather than growing the list without end.
const MAX_MARKS: usize = 4096;

impl LineMarks {
    /// The stored offset line start begins at, when a mark covers it. Lines
    /// count from one, and line one begins at the start of the block, so only
    /// the strides after it are kept: entry k holds line (k + 1) times the
    /// stride.
    pub(super) fn get(&self, line: usize) -> Option<u64> {
        if line == 0 || !line.is_multiple_of(MARK_STRIDE) {
            return None;
        }
        self.marks.get(line / MARK_STRIDE - 1).copied()
    }

    /// The latest mark at or before this line, with the line it belongs to.
    /// A scan from it reaches the wanted line in at most one stride of lines,
    /// so the furthest mark is never the answer when an earlier one is
    /// closer to the line asked for.
    pub(super) fn at_or_before(&self, line: usize) -> Option<(usize, u64)> {
        let covered = (line / MARK_STRIDE).min(self.marks.len());
        let index = covered.checked_sub(1)?;
        Some(((index + 1) * MARK_STRIDE, self.marks[index]))
    }

    /// Remember where a line starts. Marks arrive in line order, and the
    /// call ignores one that would not extend the list.
    pub(super) fn record(&mut self, line: usize, stored: u64) {
        if line == 0 || !line.is_multiple_of(MARK_STRIDE) || self.marks.len() >= MAX_MARKS {
            return;
        }
        if line / MARK_STRIDE == self.marks.len() + 1 {
            self.marks.push(stored);
        }
    }
}

/// One result the fold keeps: the row a listing returns, the block behind it,
/// and the line marks its reads have built.
#[derive(Debug, Clone)]
pub(super) struct IndexedResult {
    /// The listing row.
    pub entry: ResultEntry,
    /// Where the bytes are.
    pub source: BlockSource,
    /// The line marks built so far, shared so a read can extend them without
    /// holding the index lock.
    pub marks: Arc<Mutex<LineMarks>>,
}

impl IndexedResult {
    /// Build one indexed result with no line marks yet.
    pub(super) fn new(entry: ResultEntry, source: BlockSource) -> Self {
        Self {
            entry,
            source,
            marks: Arc::new(Mutex::new(LineMarks::default())),
        }
    }
}
