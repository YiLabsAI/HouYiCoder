//! The storage interface: ContextBackend trait + ContextError. Split from
//! lib.rs so the wire types (TurnEvent / SessionId / ...) and the storage
//! interface live in separate modules, each under the size gate.

use houyicoder_async::PFut;

use crate::{BlockHash, CheckpointId, CheckpointManifest, EventId, SessionId, TurnEvent};

/// A lenient whole-log read: the parsed events plus a count of lines
/// skipped (corrupt JSON). The search snapshot uses this so a single bad
/// line does not blank the whole search view; the strict replay path
/// stays separate (replay errors on a bad line).
#[derive(Debug, Default, Clone)]
pub struct LenientRead {
    pub events: Vec<TurnEvent>,
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

/// A reverse line batch: complete JSONL lines ending at or before from_byte,
/// in reverse (newest-first) order, each with its byte offset, plus the byte
/// offset to continue backward from (None at BOF). 64KB chunks with raw-byte
/// carry across boundaries so a multi-byte UTF-8 sequence split by a chunk
/// edge is not corrupted.

#[derive(Debug, Default, Clone)]
pub struct ReverseRead {
    /// (byte offset of the line's start, raw line text), reverse order.
    pub lines: Vec<(u64, String)>,
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

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io => write!(f, "context backend io error"),
            Self::NotFound => write!(f, "not found"),
            Self::Corrupt(msg) => write!(f, "corrupt log: {msg}"),
            Self::Unsupported => write!(f, "unsupported by this backend"),
        }
    }
}

impl std::error::Error for ContextError {}

/// The pluggable, deny-by-default storage interface. Append-only event log
/// plus checkpoint (compaction plan) storage plus an optional CAS for large
/// blobs. Object-safe (PFut) so a real async-fs / sqlite / cloud backend
/// swaps in behind Box<dyn ContextBackend>. v0: InMemoryBackend and
/// LocalFileBackend (in the memory layer). CAS methods default to Unsupported.
pub trait ContextBackend: Send + Sync {
    /// Append one event. The id is the caller's; dedup is the backend's
    /// (a duplicate id is a no-op, not an error — main-chain invariant).
    fn append(&self, event: TurnEvent) -> PFut<'_, Result<EventId, ContextError>>;

    /// Read events whose id falls in [from, to), in append order. None bounds
    /// mean open-ended. The log is append-ordered; ids are monotonic in
    /// practice (ULID) but not guaranteed within a millisecond, so callers must
    /// not assume id-sorted output.
    fn read_range(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<TurnEvent>, ContextError>>;

    /// Read the full event log for a session in append (replay) order.
    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<TurnEvent>, ContextError>>;

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
    fn read_log(&self, _session: SessionId) -> Result<Vec<TurnEvent>, ContextError> {
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

    /// Read complete lines in REVERSE from from_byte (the line ending at
    /// from_byte is the first returned). 64KB chunks with raw-byte carry
    /// across boundaries (UTF-8 safe). Returns lines (newest-first) + their
    /// byte offsets + next_from (None at BOF). The lazy offset index + the G
    /// full scan both build on this. Default empty (no on-disk log).
    fn read_lines_reverse(
        &self,
        _session: SessionId,
        _from_byte: u64,
        _max_bytes: u64,
    ) -> ReverseRead {
        ReverseRead::default()
    }
}
