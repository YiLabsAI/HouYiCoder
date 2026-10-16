//! The transcript-snapshot port: a loader the durable session log
//! implements, as the trajectory view does for timeline data. The TUI owns
//! the contract (this trait); the CLI implementation owns the session-log
//! access and the fold. The TUI never touches the log file or the
//! event types.
//!
//! Distinct from the deleted SearchLog: that was a searcher (ran the
//! query on the disk side, which caused index!=render drift). This is a
//! loader -- it returns rendered lines, and the search runs on the
//! rendered rows in the TUI, so a query never misses synthesized
//! text. The windowed successor is the same position's next generation
//! (load -> window), not a parallel path.

use crate::records::TranscriptLine;
use crate::transcript::rows::NamedRows;

/// What a load returned: the materialized lines, how many log lines were
/// skipped (corrupt), and whether the load truncated (log over the
/// threshold -- only the recent rows were kept).
#[derive(Debug, Default, Clone)]
pub struct SnapshotLoad {
    /// The rendered TranscriptLine rows the search view holds.
    pub lines: Vec<TranscriptLine>,
    /// Corrupt log lines skipped during parse (zero on the strict path).
    pub skipped: usize,
    /// True when the log exceeded the size bound and only recent rows were
    /// kept (the degrade hint shows "search limited to recent rows").
    pub truncated: bool,
}

/// A window of rendered lines from the log, anchored at a byte offset. The
/// windowed path seeks here + parses events per screen, never loading the
/// whole log. Line-aligned (never splits mid-UTF-8).
#[derive(Debug, Default, Clone)]
pub struct WindowLoad {
    /// The rows this window holds (forward order), each beside the identity
    /// the log named it with: an older-row read matches the visible front
    /// against those names, so a join between two windows lands on the row the
    /// entry named rather than on the first row whose text agrees.
    pub rows: NamedRows,
    /// The byte offset where the FIRST line in this window begins. Scrolling
    /// older loads a window ending here; the byte-% position indicator divides
    /// this by the frozen file size.
    pub start_offset: u64,
    /// The byte offset just past the LAST line in this window. Scrolling
    /// newer loads a window starting here. Equals the file size for a tail
    /// window (past EOF == nothing newer).
    pub next_offset: u64,
    /// Corrupt lines skipped in this window.
    pub skipped: usize,
    /// The total log file size (for the byte-percent position indicator).
    pub bytes_total: u64,
}

impl WindowLoad {
    /// The window's rows alone, for the search view, which reads the rendered
    /// text and names no row.
    pub fn into_lines(self) -> Vec<TranscriptLine> {
        self.rows.into_lines()
    }
}

/// An IO failure reading a window from the durable log. Unlike an empty
/// window, which means the log holds nothing older, this leaves the disk
/// front open for a retry.
#[derive(Debug, Clone)]
pub enum ReadError {
    Io,
}

/// How far the reverse scan of the log has gotten. The G key triggers a full
/// scan: it discovers where each history began and gives the log's sidecar
/// offset index its one check and rebuild. Offset queries answer from the
/// sidecar whether or not the scan has run.
#[derive(Debug, Default, Clone)]
pub struct ScanProgress {
    /// Bytes of the log the reverse scan has read (from the tail backward).
    pub scanned_bytes: u64,
    /// Total log file size.
    pub total_bytes: u64,
    /// True when the scan reached the beginning of the log.
    pub done: bool,
}

/// A loader (not a searcher) backed by the durable session log. The TUI
/// decides the threshold and degrade from log_size; the implementation only
/// produces bytes/events and folds them. Search stays in the TUI on the
/// rendered rows, so synthesized text is never missed.
///
/// The trait evolves load (read-whole, for logs under the threshold) into
/// window (byte-anchored screen reads, for logs over the threshold) -- same
/// position, not a parallel path. The log's sidecar offset index (byte_at)
/// lets the windowed view seek to any scroll position without reading the
/// prefix; G triggers a full scan with progress, which discovers the history
/// epochs and gives the sidecar its one check and rebuild.
pub trait TranscriptSnapshot: Send + Sync {
    /// The raw on-disk log size in bytes. Cheap stat the TUI uses to pick
    /// the read-whole path vs the degrade hint, without loading first.
    fn log_size(&self) -> u64;

    /// Load up to max_bytes of the log as rendered lines. For logs under
    /// the threshold; over it, returns empty + truncated (degrade).
    fn load(&self, max_bytes: u64) -> SnapshotLoad;

    /// Read a window of the log starting at byte anchor, up to max_bytes.
    /// Returns the rendered lines + the byte range [start_offset, next_offset)
    /// the window spans + corrupt count. Line-aligned (skips a partial first
    /// line).
    ///
    /// Required rather than defaulted: log_size alone routes an implementation
    /// into the windowed path, so an empty default would answer a large log
    /// with nothing instead of failing to compile. A source with no on-disk log
    /// says so by name, not by omission.
    fn window(&self, anchor: u64, max_bytes: u64) -> WindowLoad;

    /// The newest events as a window: forward-order rendered lines + the byte
    /// range they span. For opening an over-threshold log at the tail (the
    /// most recent discussion lands in view). start_offset is where the
    /// oldest line begins (scroll older from here); next_offset equals the
    /// file size (past EOF -- nothing newer). An Err is an IO failure; an
    /// empty Ok is a true empty tail.
    fn tail_window(&self, max_bytes: u64) -> Result<WindowLoad, ReadError>;

    /// The events immediately OLDER than from_byte: reverse-read the lines
    /// ending at from_byte, return them in forward (oldest-first) order with
    /// their byte range. For the n=older scan: when the current window has no
    /// older match, load the prior window here. start_offset is where the
    /// oldest line begins (continue older from here); next_offset is where
    /// this window ends (== from_byte, the newer window's start). An Err is an
    /// IO failure, not the log's end.
    fn window_before(&self, from_byte: u64, max_bytes: u64) -> Result<WindowLoad, ReadError>;

    /// Run the next chunk of the log's full reverse scan (for the G full
    /// scan, or lazily as the user scrolls). Called per frame; returns
    /// progress so the status bar shows scan percent. No-op when done. The
    /// scan discovers where histories began and checks the log's sidecar
    /// offset index; record offsets come from that index, not from the scan.
    fn scan_chunk(&self) -> ScanProgress;

    /// The byte offset of the event at event_idx, from the log's sidecar
    /// offset index. None when the index is missing or refused against the
    /// log, which degrades a seek to a walk rather than answering it wrong.
    fn byte_at(&self, event_idx: usize) -> Option<u64>;

    /// The total event count, from the log's sidecar offset index and a
    /// bounded walk past its last entry. None under the same conditions.
    fn event_count(&self) -> Option<usize>;
}

/// A source with no on-disk log: every window is empty and nothing is scanned.
///
/// A trait implementation that answers nothing does so through this type, so a
/// reader sees a name that says the source has no windows rather than an
/// omission that reads like an unfinished implementation.
pub struct NoTranscriptSnapshot;

impl TranscriptSnapshot for NoTranscriptSnapshot {
    fn log_size(&self) -> u64 {
        0
    }
    fn load(&self, _max_bytes: u64) -> SnapshotLoad {
        SnapshotLoad::default()
    }
    fn window(&self, _anchor: u64, _max_bytes: u64) -> WindowLoad {
        WindowLoad::default()
    }
    fn tail_window(&self, _max_bytes: u64) -> Result<WindowLoad, ReadError> {
        Ok(WindowLoad::default())
    }
    fn window_before(&self, _from_byte: u64, _max_bytes: u64) -> Result<WindowLoad, ReadError> {
        Ok(WindowLoad::default())
    }
    fn scan_chunk(&self) -> ScanProgress {
        ScanProgress::default()
    }
    fn byte_at(&self, _event_idx: usize) -> Option<u64> {
        None
    }
    fn event_count(&self) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests;
