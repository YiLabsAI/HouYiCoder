//! The storage interface: ContextBackend trait + ContextError. Split from
//! lib.rs so the wire types (SessionLogEntry / SessionId / ...) and the storage
//! interface live in separate modules, each under the size gate.

use houyicoder_async::PFut;

use crate::{
    BlockHash, CheckpointId, CheckpointManifest, EventId, LogIndexEntry, SessionId, SessionLogEntry,
};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::Path;

/// A lenient whole-log read: the parsed events plus a count of lines
/// skipped (corrupt JSON). The search snapshot uses this so a single bad
/// line does not blank the whole search view; the strict replay path
/// stays separate (replay errors on a bad line).
#[derive(Debug, Default, Clone)]
pub struct LenientRead {
    pub events: Vec<SessionLogEntry>,
    pub skipped: usize,
}

/// A forward line-aligned window: complete JSONL lines starting at or after
/// byte_offset, the byte offset where the next window begins (just past the
/// last complete line), and the total file size. If byte_offset lands mid-line
/// the first partial line is skipped to the next b'\n' so a returned line is
/// always complete (UTF-8 safe -- never split mid-sequence).
#[derive(Debug, Default, Clone)]
pub struct LogRangeRead {
    /// (byte offset of the line's start, raw line text), forward order.
    pub lines: Vec<(u64, String)>,
    /// Seek here for the next window (just past the last complete line; equals
    /// byte_offset if no complete line fit in max_bytes).
    pub next_offset: u64,
    /// Total log file size in bytes.
    pub bytes_total: u64,
}

/// A reverse line batch: whole JSONL lines ending at or before from_byte, in
/// reverse (newest-first) order, each with its byte offset, plus the byte
/// offset to continue backward from (None at BOF). Lines are split on their
/// terminator, so a line the log holds in parts never reaches a caller, and
/// a line wider than the read's byte budget stays out of the batch: its head
/// is above the oldest byte the read holds. The newest_line_returned flag
/// reports whether the batch reached the newest line, which is the one place
/// a caller asking for the log's last line can be misled.
#[derive(Debug, Default, Clone)]
pub struct ReverseRead {
    /// (byte offset of the line's start, raw line text), reverse order.
    pub lines: Vec<(u64, String)>,
    /// True when lines.first() is the newest whole line the log holds at or
    /// below from_byte, so a caller after the log's last line can take
    /// lines.first() as it. False when the batch holds no line at all, or
    /// holds lines older than that one: the read's byte budget ran out on bytes
    /// holding no terminator before reaching the line, so no read of that
    /// budget returns it. Trailing bytes no terminator follows hold no line, so
    /// an unterminated tail costs the flag nothing once a read reaches the last
    /// whole line before it; the one exception is a log that is nothing but
    /// such bytes, which comes back as its one line.
    pub newest_line_returned: bool,
    /// None at BOF (the whole prefix is read); else continue backward here.
    pub next_from: Option<u64>,
}

/// Errors a context backend can return.
#[derive(Debug)]
pub enum ContextError {
    /// A file or storage IO failure.
    Io,
    /// Session, checkpoint, or block not found.
    NotFound,
    /// Hash-chain break, tool_use/tool_result pair orphaned, or bad framing.
    Corrupt(String),
    /// This backend does not implement the method (e.g. CAS on v0 JSONL).
    Unsupported,
}

impl Display for ContextError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io => write!(f, "context backend io error"),
            Self::NotFound => write!(f, "not found"),
            Self::Corrupt(msg) => write!(f, "corrupt log: {msg}"),
            Self::Unsupported => write!(f, "unsupported by this backend"),
        }
    }
}

impl Error for ContextError {}

/// The pluggable, deny-by-default storage interface. Append-only event log
/// plus checkpoint (compaction plan) storage plus an optional CAS for large
/// blobs. Object-safe (PFut) so a real async-fs / sqlite / cloud backend
/// swaps in behind Box<dyn ContextBackend>. v0: InMemoryBackend and
/// LocalFileBackend (in the memory layer). CAS methods default to Unsupported.
pub trait ContextBackend: Send + Sync {
    /// Append one event. The id is the caller's; dedup is the backend's
    /// (a duplicate id is a no-op, not an error — main-chain invariant).
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>>;

    /// Read events whose id falls in [from, to), in append order. None bounds
    /// mean open-ended. The log is append-ordered; ids are monotonic in
    /// practice (ULID) but not guaranteed within a millisecond, so callers must
    /// not assume id-sorted output.
    fn read_range(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>>;

    /// Read the full event log for a session in append (replay) order.
    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>>;

    /// Persist a compaction plan + summary. Append-only: a new checkpoint does
    /// not delete earlier ones (rewind points).
    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>>;

    /// Read a checkpoint by id.
    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>>;

    /// List checkpoint ids for a session, oldest first.
    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>>;

    /// Store a large blob in the CAS and return its hash. Takes owned bytes so
    /// a real async backend moves them into the future without cloning the
    /// very blobs the CAS exists to dedup. Default Unsupported (v0 JSONL).
    fn block_put(&self, _block: Vec<u8>) -> PFut<'_, Result<BlockHash, ContextError>> {
        Box::pin(async move { Err(ContextError::Unsupported) })
    }

    /// Retrieve a blob by hash. Default Unsupported.
    fn block_get(&self, _hash: &BlockHash) -> PFut<'_, Result<Vec<u8>, ContextError>> {
        Box::pin(async move { Err(ContextError::Unsupported) })
    }

    /// Whether this backend can serve byte-anchored windows of the log.
    ///
    /// A caller that pages history has to know whether windows are available
    /// before it reads one, and the answer is a property of the backend rather
    /// than of the session: a file backend serves windows for a session whose
    /// log is still empty, and an in-memory backend never does. Deciding this
    /// from log_size would confuse the two and cost a stat per call.
    fn supports_log_windows(&self) -> bool {
        false
    }

    /// The raw on-disk log size in bytes for a session, for the cheap
    /// threshold check the search snapshot does before deciding to load the
    /// whole log vs degrade. A backend with no on-disk log (in-memory) returns
    /// 0; callers treat 0 as "no disk log to snapshot".
    fn log_size(&self, _session: SessionId) -> u64 {
        0
    }

    /// Read the full event log synchronously. The search snapshot loads the
    /// whole log into a TranscriptLine snapshot on the TUI's sync render path,
    /// so it cannot drive the async replay future. Strict: a corrupt line
    /// errors (the lenient read is the default below, not this).
    /// Default Unsupported (backends with no on-disk log).
    fn read_log(&self, _session: SessionId) -> Result<Vec<SessionLogEntry>, ContextError> {
        Err(ContextError::Unsupported)
    }

    /// Read the whole log leniently: parse what parses, skip + count lines
    /// that don't. The snapshot search view uses this so one bad line does
    /// not blank the view; the chrome surfaces the skip count. The strict
    /// replay path is NOT this (replay errors on a bad line) -- two paths,
    /// not one helper. Default: delegate to read_log (Ok -> events + 0
    /// skipped, Err -> empty). Backends with on-disk logs override to
    /// skip corrupt lines individually.
    fn read_log_lenient(&self, session: SessionId) -> LenientRead {
        match self.read_log(session) {
            Ok(events) => LenientRead { events, skipped: 0 },
            Err(_) => LenientRead::default(),
        }
    }

    /// Read a forward line-aligned window of the log starting at byte_offset,
    /// up to max_bytes. The first partial line (if byte_offset lands mid-line)
    /// is skipped to the next b'\n' so every returned line is complete. The
    /// byte-window search view seeks here + parses ~50 events/screen. Default
    /// empty (backends with no on-disk log).
    fn read_log_range(
        &self,
        _session: SessionId,
        _byte_offset: u64,
        _max_bytes: u64,
    ) -> LogRangeRead {
        LogRangeRead::default()
    }

    /// Read whole lines in REVERSE from from_byte: the newest line the log
    /// holds that ends at or below from_byte comes back first. Lines are
    /// split on their terminator, so a line the log holds in parts never
    /// reaches a caller, and a line wider than max_bytes stays out of the
    /// batch: every read of that budget fails to reach its head. next_from is
    /// where a walk resumes: None at BOF, else the boundary below which no
    /// byte is left over, so a walk in reads smaller than the log gives each
    /// line back once, whole. newest_line_returned reports whether the batch
    /// reached the log's newest line at or below from_byte. The epoch scan
    /// and the window lookback both build on this.
    ///
    /// Three outcomes a caller must keep apart. An empty batch with next_from
    /// None is the log's end: the byte range held no whole line, or the walk
    /// reached BOF, and the caller latches the front closed. An Err of Io is a
    /// read that failed against a log the backend does hold: the caller must
    /// not latch the front closed as if the log had ended, but leave it open
    /// for a retry after a cooldown. An Err of Unsupported is a backend with no
    /// byte-addressable log (an in-memory store): the caller has no disk
    /// window to read, which is not a failure to retry. The default returns
    /// Unsupported, because a backend that does not override this has no byte
    /// log to read.
    fn read_lines_reverse(
        &self,
        _session: SessionId,
        _from_byte: u64,
        _max_bytes: u64,
    ) -> Result<ReverseRead, ContextError> {
        Err(ContextError::Unsupported)
    }

    /// The offset-index entry covering a record position: the sampled record
    /// with the largest ordinal at or below the one asked for, from which a
    /// forward walk reaches it. None when the backend keeps no index on disk,
    /// or when the index fails its checks against the log (missing, corrupt,
    /// or built over a log that was shortened or replaced). A query then
    /// degrades to a walk or to no answer, never to a wrong offset.
    fn log_index_floor(&self, _session: SessionId, _ordinal: u64) -> Option<LogIndexEntry> {
        None
    }

    /// Rebuild a session's offset index from its log in one streaming pass.
    /// Called on a background scan when the index is missing or refused, not
    /// on a query path. True reports that index queries can be answered
    /// after the call, or that the log holds no record to index. Default
    /// rebuilds nothing and answers false (a backend with no on-disk index).
    fn rebuild_log_index(&self, _session: SessionId) -> bool {
        false
    }

    /// The sessions root this backend persists under, for cross-session
    /// readers (the dream's retry scan). Derived, not configured, so the
    /// reader can never disagree with the writer about the root: a
    /// disk-backed build exposes its own root, an in-memory build returns
    /// None and carries no cross-session history at all - it must not read
    /// the real home. Default None (no on-disk log).
    fn session_log_root(&self) -> Option<&Path> {
        None
    }
}
