//! LocalFileBackend: persistent ContextBackend for v0. One directory per
//! session under a root holds log.jsonl (append-only, 0o600), checkpoints,
//! and log.index; reads and writes resolve a session's directory from either
//! the uuid name or the ULID name a store written before that format used.
//! Append dedup uses a seen set built by one streaming read on first append
//! and then O(1) per append, and that pass also builds the offset index.
//! Checkpoints are per-file, so listing scans filenames. Sync std::fs runs
//! inside Box::pin(async), fine for prototype sessions.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
#[cfg(test)]
use std::sync::atomic::Ordering;

#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;

use houyicoder_async::PFut;
use houyicoder_context::session_class::{CHECKPOINT_DIR, INDEX_FILE, LOG_FILE, session_dir};
use houyicoder_context::{
    BlockHash, BlockWindow, CheckpointId, CheckpointManifest, ContextBackend, ContextError,
    EventId, LenientRead, LogIndexEntry, LogRangeRead, ReverseRead, SessionId, SessionLogEntry,
};

mod blocks;
mod checkpoints;
mod index_check;
mod log_index;
mod log_windows;
mod records;
pub use blocks::{ReclaimError, ReclaimPolicy, ReclaimReport, ReclaimStop};
#[cfg(unix)]
use fs::DirBuilder;
use log_index::SessionWriteState;

#[cfg(test)]
mod window;

pub struct LocalFileBackend {
    root: PathBuf,
    /// Per-session write state so append dedup is an O(1) set lookup, not an
    /// O(n) full-log rescan. Cold on the first append to a session, built from
    /// the existing log once and held for the life of the backend.
    write_state: Mutex<HashMap<SessionId, SessionWriteState>>,
    /// Number of times load_write_state read the log for dedup. A warm cache
    /// keeps this at one per session; a regression to per-append rescans
    /// would push it to one per append. Test-only observability hook.
    read_count: AtomicUsize,
    /// Number of lines converted one at a time because their window held bytes
    /// that are not valid UTF-8. A window of valid lines converts in the single
    /// check and never touches this; only a damaged window walks the per-line
    /// conversion. Test-only observability hook: both conversions return the
    /// same text on valid input, so counting them is the only way a test can
    /// tell the single check from the per-line fallback.
    lossy_line_count: AtomicUsize,
    /// The positive first-record identities, by session. An append-only log
    /// never changes its first record, so one confirmed identity answers every
    /// later index query without re-reading the log's leading mebibyte. Only
    /// positive identities are cached: an empty log can gain a first record,
    /// and a failed read can succeed later.
    first_record_ids: Mutex<HashMap<SessionId, EventId>>,
    /// The file identity each read block was last confirmed under. Checking a
    /// block against its address means hashing it whole, which a windowed read
    /// cannot afford per page; this remembers the check so later windows of
    /// the same unchanged file skip it.
    verified_blocks: Mutex<HashMap<BlockHash, blocks::BlockStamp>>,
}

impl LocalFileBackend {
    /// Construct a backend rooted at root. The root and per-session dirs are
    /// created 0o700 on demand.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            write_state: Mutex::new(HashMap::new()),
            read_count: AtomicUsize::new(0),
            lossy_line_count: AtomicUsize::new(0),
            first_record_ids: Mutex::new(HashMap::new()),
            verified_blocks: Mutex::new(HashMap::new()),
        }
    }

    /// Number of times the log was read for dedup (the rescan count). One
    /// per session when the seen cache works; one per append on a regression.
    #[cfg(test)]
    pub(crate) fn dedup_read_count(&self) -> usize {
        self.read_count.load(Ordering::Relaxed)
    }

    /// Number of lines the per-line conversion produced. Zero after a read
    /// means the window validated in one check.
    #[cfg(test)]
    pub(crate) fn lossy_line_count(&self) -> usize {
        self.lossy_line_count.load(Ordering::Relaxed)
    }

    pub(crate) fn log_path(&self, session: SessionId) -> PathBuf {
        session_dir(&self.root, session).join(LOG_FILE)
    }

    fn index_path(&self, session: SessionId) -> PathBuf {
        session_dir(&self.root, session).join(INDEX_FILE)
    }

    fn checkpoint_dir(&self, session: SessionId) -> PathBuf {
        session_dir(&self.root, session).join(CHECKPOINT_DIR)
    }

    fn checkpoint_path(&self, session: SessionId, cp: CheckpointId) -> PathBuf {
        self.checkpoint_dir(session).join(format!("{cp}.json"))
    }

    fn ensure_dir(path: &Path) -> Result<(), ContextError> {
        #[cfg(unix)]
        {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|_| ContextError::Io)
        }
        #[cfg(not(unix))]
        {
            fs::create_dir_all(path).map_err(|_| ContextError::Io)
        }
    }
}

impl ContextBackend for LocalFileBackend {
    fn supports_log_windows(&self) -> bool {
        true
    }

    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        let id = self.append_sync(event);
        Box::pin(async move { id })
    }

    fn read_range(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        let out = self.read_range_sync(session, from, to);
        Box::pin(async move { out })
    }

    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        let out = self.replay_sync(session);
        Box::pin(async move { out })
    }

    fn log_size(&self, session: SessionId) -> u64 {
        // A missing log (new session, never appended) is size 0; the snapshot
        // caller treats 0 as "no disk log" and skips the load.
        fs::metadata(self.log_path(session))
            .map(|m| m.len())
            .unwrap_or(0)
    }

    fn session_log_root(&self) -> Option<&Path> {
        Some(&self.root)
    }

    fn read_log(&self, session: SessionId) -> Result<Vec<SessionLogEntry>, ContextError> {
        // Strict: a corrupt line errors here. The snapshot's tolerant read
        // (skip + count) is a separate path, not this trait method.
        self.read_log(session)
    }

    fn read_log_lenient(&self, session: SessionId) -> LenientRead {
        self.read_log_lenient_sync(session)
    }

    fn read_log_range(&self, session: SessionId, byte_offset: u64, max_bytes: u64) -> LogRangeRead {
        self.read_byte_range(session, byte_offset, max_bytes)
    }

    fn read_lines_reverse(
        &self,
        session: SessionId,
        from_byte: u64,
        max_bytes: u64,
    ) -> Result<ReverseRead, ContextError> {
        self.read_reverse_lines(session, from_byte, max_bytes)
    }

    fn log_index_floor(&self, session: SessionId, ordinal: u64) -> Option<LogIndexEntry> {
        self.index_floor(session, ordinal)
    }

    fn rebuild_log_index(&self, session: SessionId) -> bool {
        self.rebuild_index(session)
    }

    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        let id = self.write_checkpoint_sync(manifest);
        Box::pin(async move { id })
    }

    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        let out = self.read_checkpoint_sync(id);
        Box::pin(async move { out })
    }

    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        let out = self.list_checkpoints_sync(session);
        Box::pin(async move { out })
    }

    fn block_put(&self, block: Vec<u8>) -> PFut<'_, Result<BlockHash, ContextError>> {
        let out = self.block_put_sync(block);
        Box::pin(async move { out })
    }

    fn block_get(&self, hash: &BlockHash) -> PFut<'_, Result<Vec<u8>, ContextError>> {
        let out = self.block_get_sync(hash);
        Box::pin(async move { out })
    }

    fn block_get_range(
        &self,
        hash: &BlockHash,
        offset: u64,
        len: usize,
    ) -> PFut<'_, Result<BlockWindow, ContextError>> {
        let out = self.block_get_range_sync(hash, offset, len);
        Box::pin(async move { out })
    }
}
