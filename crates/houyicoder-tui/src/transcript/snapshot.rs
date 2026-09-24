//! The transcript-snapshot port: a loader the durable session log
//! implements, as the trajectory view does for timeline data. The TUI owns
//! the contract (this trait); the CLI implementation owns the session-log
//! access and the projection. The TUI never touches the log file or the
//! event types.
//!
//! Distinct from the deleted SearchLog: that was a searcher (ran the
//! query on the disk side, which caused index!=render drift). This is a
//! loader -- it returns rendered lines, and the search runs on the
//! rendered projection in the TUI, so a query never misses synthesized
//! text. The windowed successor is the same position's next generation
//! (load -> window), not a parallel path.

use crate::records::TranscriptLine;

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
    /// The rendered TranscriptLine rows in this window (forward order).
    pub lines: Vec<TranscriptLine>,
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

/// How much of the event-byte-offset index is built. The G key triggers a
/// full build (reverse-reading the whole log); until then the index only
/// covers what the user has scrolled to (lazy, like less).
#[derive(Debug, Default, Clone)]
pub struct IndexProgress {
    /// Bytes of the log indexed so far (from the tail backward).
    pub indexed_bytes: u64,
    /// Total log file size.
    pub total_bytes: u64,
    /// True when the full index is built.
    pub done: bool,
}

/// A loader (not a searcher) backed by the durable session log. The TUI
/// decides the threshold and degrade from log_size; the implementation only
/// produces bytes/events and projects them. Search stays in the TUI on the
/// rendered projection, so synthesized text is never missed.
///
/// The trait evolves load (read-whole, for logs under the threshold) into
/// window (byte-anchored screen reads, for logs over the threshold) -- same
/// position, not a parallel path. The lazy offset index (index_chunk +
/// byte_at) lets the windowed view seek to any scroll position without
/// reading the prefix; G triggers a full build with progress.
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
    /// file size (past EOF -- nothing newer).
    fn tail_window(&self, max_bytes: u64) -> WindowLoad;

    /// The events immediately OLDER than from_byte: reverse-read the lines
    /// ending at from_byte, return them in forward (oldest-first) order with
    /// their byte range. For the n=older scan: when the current window has no
    /// older match, load the prior window here. start_offset is where the
    /// oldest line begins (continue older from here); next_offset is where
    /// this window ends (== from_byte, the newer window's start).
    fn window_before(&self, from_byte: u64, max_bytes: u64) -> WindowLoad;

    /// Build the next chunk of the event-byte-offset index (for the G full
    /// scan, or lazily as the user scrolls). Called per frame; returns
    /// progress so the status bar shows indexing percent. No-op when done.
    fn index_chunk(&self) -> IndexProgress;

    /// The byte offset of the event at event_idx (from the index). None if
    /// the index does not cover that position yet.
    fn byte_at(&self, event_idx: usize) -> Option<u64>;

    /// The total event count (from the index). None if not yet built.
    fn event_count(&self) -> Option<usize>;
}

/// A source with no on-disk log: every window is empty and nothing is indexed.
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
    fn tail_window(&self, _max_bytes: u64) -> WindowLoad {
        WindowLoad::default()
    }
    fn window_before(&self, _from_byte: u64, _max_bytes: u64) -> WindowLoad {
        WindowLoad::default()
    }
    fn index_chunk(&self) -> IndexProgress {
        IndexProgress::default()
    }
    fn byte_at(&self, _event_idx: usize) -> Option<u64> {
        None
    }
    fn event_count(&self) -> Option<usize> {
        None
    }
}
