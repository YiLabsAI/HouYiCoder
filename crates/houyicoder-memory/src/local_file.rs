//! LocalFileBackend: persistent ContextBackend for v0. One directory per
//! session under a root: <root>/<session>/log.jsonl (append-only, 0o600) +
//! <root>/<session>/checkpoints/<cp>.json. File mode 0o600, dir 0o700 (Unix).
//!
//! Main-chain dedup by event id via an in-memory seen set, mirroring
//! InMemoryBackend. The set is cold on first append to a session: one
//! streaming read of the existing log builds it, then each append is an
//! O(1) set lookup. Without the cache every append re-read the whole log
//! (O(n) per append, O(n^2) over a session -- a 500MB log pays 500MB per
//! write). Checkpoints are per-file so list_checkpoints scans filenames,
//! not contents.
//!
//! v0 uses sync std::fs inside Box::pin(async) (consistent with InMemoryBackend).
//! This blocks the async runtime on IO; a production impl switches to tokio::fs
//! or a spawned blocking task. Fine for prototype sessions.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

use houyicoder_async::PFut;
use houyicoder_context::{
    BlockHash, CheckpointId, CheckpointManifest, ContextBackend, ContextError, EventId,
    LenientRead, LogRangeRead, ReverseRead, SessionId, SessionLogEntry,
};

use crate::sha256_hex;

#[cfg(test)]
mod window;

pub struct LocalFileBackend {
    root: PathBuf,
    /// Per-session seen-id cache so append dedup is an O(1) set lookup,
    /// not an O(n) full-log rescan. Cold on first append to a session:
    /// load_seen_set builds it from the existing log once, like
    /// InMemoryBackend.seen.
    seen: Mutex<HashMap<SessionId, HashSet<EventId>>>,
    /// Number of times load_seen_set read the log for dedup. A warm cache
    /// keeps this at one per session; a regression to per-append rescans
    /// would push it to one per append. Test-only observability hook.
    read_count: AtomicUsize,
}

impl LocalFileBackend {
    /// Construct a backend rooted at root. The root and per-session dirs are
    /// created 0o700 on demand.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            seen: Mutex::new(HashMap::new()),
            read_count: AtomicUsize::new(0),
        }
    }

    /// Number of times the log was read for dedup (the rescan count). One
    /// per session when the seen cache works; one per append on a regression.
    #[cfg(test)]
    pub(crate) fn dedup_read_count(&self) -> usize {
        self.read_count.load(Ordering::Relaxed)
    }

    /// Build the seen-id set for a session by streaming its log line by
    /// line (avoids reading a 500MB log into one heap allocation). Called
    /// once on the first append to a cold session; cached in self.seen.
    /// Increments read_count so a per-append rescan regression is visible.
    fn load_seen_set(&self, session: SessionId) -> HashSet<EventId> {
        self.read_count.fetch_add(1, Ordering::Relaxed);
        let log = self.log_path(session);
        let Ok(f) = fs::File::open(&log) else {
            return HashSet::new();
        };
        let reader = BufReader::new(f);
        let mut set = HashSet::new();
        for line in reader.lines() {
            let Ok(line) = line else {
                break;
            };
            if line.is_empty() {
                continue;
            }
            // Parse only to read the id; a corrupt line is skipped (a
            // later read_log surfaces it as Corrupt). Tolerating here keeps
            // a single bad line from blocking the whole session's dedup.
            if let Ok(event) = serde_json::from_str::<SessionLogEntry>(&line) {
                set.insert(event.id);
            }
        }
        set
    }

    fn session_dir(&self, session: SessionId) -> PathBuf {
        self.root.join(format!("{session}"))
    }

    pub(crate) fn log_path(&self, session: SessionId) -> PathBuf {
        self.session_dir(session).join("log.jsonl")
    }

    fn checkpoint_dir(&self, session: SessionId) -> PathBuf {
        self.session_dir(session).join("checkpoints")
    }

    fn checkpoint_path(&self, session: SessionId, cp: CheckpointId) -> PathBuf {
        self.checkpoint_dir(session).join(format!("{cp}.json"))
    }

    // CAS blocks live under <root>/.cas, shared across sessions (content
    // addressing dedups across sessions). Sharded by the first 2 hex chars of
    // the hash to keep any one directory from growing unbounded.
    fn cas_dir(&self) -> PathBuf {
        self.root.join(".cas")
    }

    fn block_path(&self, hash: &BlockHash) -> PathBuf {
        let prefix = hash.0.get(..2).unwrap_or("");
        self.cas_dir().join(prefix).join(format!("{}.bin", hash.0))
    }

    fn ensure_dir(path: &Path) -> Result<(), ContextError> {
        #[cfg(unix)]
        {
            fs::DirBuilder::new()
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

    fn append_sync(&self, event: SessionLogEntry) -> Result<EventId, ContextError> {
        let session = event.session;
        Self::ensure_dir(&self.session_dir(session))?;
        let log = self.log_path(session);
        // Dedup: O(1) set lookup, not an O(n) full-log rescan. The set is
        // cold on the first append to a session -- load_seen_set builds it
        // once from the existing log (the resume-replay case: a fresh
        // backend over a prior session pays one read, then each replayed
        // event is a set hit). Held through the write so a failed write
        // does not pollute the set (the id is inserted only after the
        // line lands). v0 serializes appends, so holding the lock through
        // the blocking write is acceptable.
        let mut seen = self.seen.lock().expect("seen mutex poisoned");
        let set = seen
            .entry(session)
            .or_insert_with(|| self.load_seen_set(session));
        if set.contains(&event.id) {
            return Ok(event.id); // main-chain dedup: no-op.
        }
        let mut json = serde_json::to_string(&event)
            .map_err(|_| ContextError::Corrupt("event failed to serialize".into()))?;
        json.push('\n');
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut f = opts.open(&log).map_err(|_| ContextError::Io)?;
        f.write_all(json.as_bytes()).map_err(|_| ContextError::Io)?;
        // fsync so a crash after the append does not lose the event (the OS
        // page cache is not durable). The block_put + checkpoint writes fsync
        // too so the manifest never references a block that is not on disk.
        f.sync_all().map_err(|_| ContextError::Io)?;
        set.insert(event.id);
        Ok(event.id)
    }

    fn read_log(&self, session: SessionId) -> Result<Vec<SessionLogEntry>, ContextError> {
        let log = self.log_path(session);
        let content = match fs::read_to_string(&log) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(ContextError::Io),
        };
        let mut events = Vec::new();
        for line in content.lines() {
            if line.is_empty() {
                continue;
            }
            let event: SessionLogEntry = serde_json::from_str(line)
                .map_err(|e| ContextError::Corrupt(format!("bad event line: {e}")))?;
            events.push(event);
        }
        Ok(events)
    }

    fn read_range_sync(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> Result<Vec<SessionLogEntry>, ContextError> {
        let events = self.read_log(session)?;
        Ok(events
            .into_iter()
            .filter(|e| from.is_none_or(|f| e.id >= f))
            .filter(|e| to.is_none_or(|t| e.id < t))
            .collect())
    }

    fn replay_sync(&self, session: SessionId) -> Result<Vec<SessionLogEntry>, ContextError> {
        self.read_log(session)
    }

    fn write_checkpoint_sync(
        &self,
        manifest: CheckpointManifest,
    ) -> Result<CheckpointId, ContextError> {
        let session = manifest.session;
        let id = manifest.id;
        Self::ensure_dir(&self.checkpoint_dir(session))?;
        let path = self.checkpoint_path(session, id);
        let json = serde_json::to_string(&manifest)
            .map_err(|_| ContextError::Corrupt("manifest failed to serialize".into()))?;
        // Atomic write: serialize to a temp sibling, fsync, rename over the
        // target so a crash never leaves a partial/truncated manifest (a
        // reader sees either the old checkpoint or the complete new one).
        let tmp = path.with_extension("tmp");
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut f = opts.open(&tmp).map_err(|_| ContextError::Io)?;
        f.write_all(json.as_bytes()).map_err(|_| ContextError::Io)?;
        f.sync_all().map_err(|_| ContextError::Io)?;
        // fsync the directory so the rename is durable (POSIX rename is
        // atomic but the dir entry needs its own fsync).
        drop(f);
        fs::rename(&tmp, &path).map_err(|_| ContextError::Io)?;
        if let Some(dir) = path.parent()
            && let Ok(dir_file) = fs::File::open(dir)
        {
            drop(dir_file.sync_all());
        }
        Ok(id)
    }

    fn read_checkpoint_sync(&self, id: CheckpointId) -> Result<CheckpointManifest, ContextError> {
        // Checkpoints are namespaced by session; find the file by scanning the
        // root's session dirs for <session>/checkpoints/<id>.json.
        let entries = fs::read_dir(&self.root).map_err(|_| ContextError::Io)?;
        for entry in entries.flatten() {
            let cp_dir = entry.path().join("checkpoints");
            let cp_file = cp_dir.join(format!("{id}.json"));
            if cp_file.exists() {
                let content = fs::read_to_string(&cp_file).map_err(|_| ContextError::Io)?;
                return serde_json::from_str(&content)
                    .map_err(|e| ContextError::Corrupt(format!("bad checkpoint: {e}")));
            }
        }
        Err(ContextError::NotFound)
    }

    fn list_checkpoints_sync(&self, session: SessionId) -> Result<Vec<CheckpointId>, ContextError> {
        let dir = self.checkpoint_dir(session);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(ContextError::Io),
        };
        let mut ids: Vec<CheckpointId> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let name = name.to_str()?;
                let stem = name.strip_suffix(".json")?;
                CheckpointId::from_display_string(stem)
            })
            .collect();
        ids.sort();
        Ok(ids)
    }

    fn block_put_sync(&self, block: Vec<u8>) -> Result<BlockHash, ContextError> {
        let hash = sha256_hex(&block);
        let path = self.block_path(&hash);
        // Dedup: same content already on disk; do not rewrite.
        if path.exists() {
            return Ok(hash);
        }
        if let Some(parent) = path.parent() {
            Self::ensure_dir(parent)?;
        }
        // Atomic write to a temp sibling + fsync + rename so a crash never
        // leaves a partial block (a reader gets the complete block or nothing).
        let tmp = path.with_extension("bin.tmp");
        let mut opts = OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut f = opts.open(&tmp).map_err(|_| ContextError::Io)?;
        f.write_all(&block).map_err(|_| ContextError::Io)?;
        f.sync_all().map_err(|_| ContextError::Io)?;
        drop(f);
        fs::rename(&tmp, &path).map_err(|_| ContextError::Io)?;
        // Fsync the parent directory so the rename is durable across a
        // crash; otherwise the block's data is fsynced but its directory
        // entry is not, leaving a dangling block_ref on restart.
        if let Some(dir) = path.parent()
            && let Ok(dir_file) = fs::File::open(dir)
        {
            drop(dir_file.sync_all());
        }
        Ok(hash)
    }

    fn block_get_sync(&self, hash: &BlockHash) -> Result<Vec<u8>, ContextError> {
        let path = self.block_path(hash);
        match fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ContextError::NotFound),
            Err(_) => Err(ContextError::Io),
        }
    }

    /// Collect the block_ref hashes referenced across every session log so the
    /// a maintenance pass can tell a referenced block (keep) from an orphan (reclaim). Scans
    /// the JSONL for the block_ref field — a coarse text scan, not a typed
    /// parse, since the reclaim pass walks every session + a typed parse of every event
    /// is needless work for a maintenance pass.
    fn referenced_block_hashes(&self) -> std::collections::HashSet<String> {
        let mut refs = std::collections::HashSet::new();
        let Ok(sessions) = fs::read_dir(&self.root) else {
            return refs;
        };
        let needle = "\"block_ref\":\"";
        for entry in sessions.flatten() {
            let log = entry.path().join("log.jsonl");
            let Ok(content) = fs::read_to_string(&log) else {
                continue;
            };
            let mut at = 0;
            while let Some(i) = content[at..].find(needle) {
                let start = at + i + needle.len();
                let rest = &content[start..];
                let end = rest.find('"').unwrap_or(0);
                if end > 0 {
                    refs.insert(rest[..end].to_string());
                }
                at = start + end + 1;
            }
        }
        refs
    }

    /// Reclaim orphan CAS blocks: blocks on disk whose hash no session log
    /// references (the source event was compacted out + the manifest dropped
    /// the block_ref). A maintenance pass, full-scan, not hot-path. Returns
    /// the count of blocks removed. A block still referenced by any session
    /// stays; a block referenced by none (an orphan) is deleted.
    pub fn reclaim_orphan_blocks(&self) -> Result<usize, ContextError> {
        let referenced = self.referenced_block_hashes();
        let mut removed = 0;
        let Ok(shards) = fs::read_dir(self.cas_dir()) else {
            return Ok(0);
        };
        for shard in shards.flatten() {
            let Ok(blocks) = fs::read_dir(shard.path()) else {
                continue;
            };
            for block in blocks.flatten() {
                let fname = block.file_name();
                let name = match fname.to_str() {
                    Some(n) => n,
                    None => continue,
                };
                // <hash>.bin — strip the suffix for the referenced-set lookup.
                let Some(hash) = name.strip_suffix(".bin") else {
                    // A stale .bin.tmp left by a crash during block_put_sync
                    // (the temp sibling never reached the rename). No
                    // reference can exist for a temp file, so reap it
                    // unconditionally instead of leaking CAS storage.
                    if name.ends_with(".bin.tmp") && fs::remove_file(block.path()).is_ok() {
                        removed += 1;
                    }
                    continue;
                };
                if !referenced.contains(hash) && fs::remove_file(block.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

/// Whether the log's byte at the given offset ends a line. A read holds the
/// bytes of its own window, so this answers for the one byte either side of it:
/// the byte before the window's first says whether the window starts at a
/// line's head, the byte at its newest boundary whether the window's last
/// stretch is a line the log terminates. A read that fails leaves the caller
/// without the fact, which costs it the segment rather than the whole batch.
fn byte_ends_line(f: &mut fs::File, at: u64) -> bool {
    let mut one = [0u8; 1];
    f.seek(SeekFrom::Start(at)).is_ok() && f.read_exact(&mut one).is_ok() && one[0] == b'\n'
}

/// The bytes one backward read covers, oldest first, and what the log holds
/// either side of them. Splitting these bytes into lines is all the read needs:
/// an edge can fall anywhere inside a line, so the pieces are put together from
/// the window rather than from a carry that has to survive the walk.
struct LogWindow<'a> {
    bytes: &'a [u8],
    /// Absolute offset of the window's first byte.
    start: u64,
    /// The read's newest boundary: one past the window's last byte.
    from_byte: u64,
    /// The window holds the log from its first byte.
    covers_log: bool,
    /// The byte before the window ends a line, so the window's first segment
    /// has a head the log shows.
    starts_at_line: bool,
    /// The byte at from_byte ends a line, so the window's last segment is a
    /// line the log terminates without the window holding the terminator.
    ends_with_line: bool,
}

/// What a window's bytes yield: the whole lines and whether the newest whole
/// line the log holds at or below the window came back.
struct WindowLines {
    /// (byte offset of the line's start, raw line text), newest first.
    lines: Vec<(u64, String)>,
    /// True when the batch's first line is the log's newest whole line at or
    /// below the window. False when the window yields no line, or yields lines
    /// older than that one: its budget ran out on bytes holding no terminator
    /// before reaching the line, so no read of that budget returns it.
    newest_line_returned: bool,
}

impl LogWindow<'_> {
    /// The whole lines the window holds, newest first. A segment whose head or
    /// terminator the window does not show is no line yet: the walk re-reads it
    /// from a boundary that holds it, or, for a line wider than the read's
    /// budget, never.
    fn whole_lines(&self) -> WindowLines {
        let mut lines: Vec<(u64, String)> = Vec::new();
        let mut head_shown = self.starts_at_line;
        let mut start = 0usize;
        // One past the newest returned line's terminator, or past the line's
        // own end when the log holds no terminator for it. At the window's
        // start, so a window that yields no line leaves every byte of itself
        // to answer whether a terminator sits above the batch.
        let mut newest_end = self.start;
        for (i, &b) in self.bytes.iter().enumerate() {
            if b != b'\n' {
                continue;
            }
            if head_shown {
                let text = String::from_utf8_lossy(&self.bytes[start..i]).into_owned();
                newest_end = self.start + i as u64 + 1;
                lines.push((self.start + start as u64, text));
            }
            start = i + 1;
            head_shown = true;
        }
        // The bytes after the last terminator in the window: a whole line when
        // the log terminates it at the window's boundary, when they are the
        // whole log, which one line written without a terminator ends as, or
        // when the window ends on the terminator itself, which ends an empty
        // line. An empty window holds no line at all.
        if !self.bytes.is_empty() {
            let one_line_log = self.covers_log && start == 0;
            if self.ends_with_line || one_line_log {
                let text = String::from_utf8_lossy(&self.bytes[start..]).into_owned();
                newest_end = self.from_byte;
                lines.push((self.start + start as u64, text));
            }
        }
        lines.reverse();
        // A terminator among the bytes above the newest returned line ends a
        // newer line the window does not hold whole, so no read of this window
        // returns it. A window that yields no line returns none: it holds no
        // head or no terminator to end one, so a caller has nothing to take for
        // the log's newest line.
        let above = (newest_end - self.start) as usize;
        let newest_line_returned =
            !lines.is_empty() && !self.bytes[above.min(self.bytes.len())..].contains(&b'\n');
        WindowLines {
            newest_line_returned,
            lines,
        }
    }
}

impl ContextBackend for LocalFileBackend {
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
        std::fs::metadata(self.log_path(session))
            .map(|m| m.len())
            .unwrap_or(0)
    }

    fn session_log_root(&self) -> Option<&std::path::Path> {
        Some(&self.root)
    }

    fn read_log(&self, session: SessionId) -> Result<Vec<SessionLogEntry>, ContextError> {
        // Strict: a corrupt line errors here. The snapshot's tolerant read
        // (skip + count) is a separate path, not this trait method.
        self.read_log(session)
    }

    fn read_log_lenient(&self, session: SessionId) -> LenientRead {
        // Tolerant: parse what parses, skip + count bad lines. The search
        // snapshot uses this so one corrupt line does not blank the whole
        // view. NOT the strict replay path (that errors on a bad line) --
        // two paths, not one shared helper.
        let log = self.log_path(session);
        let Ok(f) = fs::File::open(&log) else {
            return LenientRead::default();
        };
        let reader = BufReader::new(f);
        let mut events = Vec::new();
        let mut skipped = 0;
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionLogEntry>(&line) {
                Ok(ev) => events.push(ev),
                Err(_) => skipped += 1,
            }
        }
        LenientRead { events, skipped }
    }
    fn read_log_range(&self, session: SessionId, byte_offset: u64, max_bytes: u64) -> LogRangeRead {
        // Forward line-aligned window: seek to byte_offset, skip a partial
        // first line (if byte_offset lands mid-line) to the next b'\n', then
        // read complete lines until max_bytes consumed. UTF-8 safe -- the
        // split is on b'\n' (a single-byte ASCII), never inside a multi-byte
        // sequence.
        let log = self.log_path(session);
        let Ok(f) = fs::File::open(&log) else {
            return LogRangeRead::default();
        };
        let mut f = f;
        let bytes_total = f.metadata().map(|m| m.len()).unwrap_or(0);
        if byte_offset >= bytes_total {
            return LogRangeRead {
                bytes_total,
                ..Default::default()
            };
        }
        if f.seek(SeekFrom::Start(byte_offset)).is_err() {
            return LogRangeRead {
                bytes_total,
                ..Default::default()
            };
        }
        let mut buf = vec![0u8; max_bytes.min(1 << 20) as usize]; // cap the read at 1MB
        let n = f.read(&mut buf).unwrap_or(0);
        buf.truncate(n);
        // Skip a partial first line when byte_offset lands mid-line (the byte
        // before it is not a newline). When byte_offset is a line start --
        // which is the case for every window after the first when the caller
        // advances by next_offset (a line-aligned offset) -- the first
        // segment is a complete line and must NOT be skipped. Reading the
        // byte at offset-1 distinguishes the two; a seek-and-read of one
        // byte is cheap (one syscall per window).
        let mut start = 0usize;
        if byte_offset != 0 {
            let mid_line = if n == 0 {
                true
            } else {
                use std::io::{Read as _, Seek as _};
                let prev_byte = f.seek(SeekFrom::Start(byte_offset - 1)).ok().and_then(|_| {
                    let mut b = [0u8; 1];
                    f.read(&mut b).ok().filter(|&r| r == 1).map(|_| b[0])
                });
                // Restore the read cursor so the line-split below sees the
                // window from byte_offset. If the seek-back failed, fall back
                // to the conservative mid-line skip.
                drop(f.seek(SeekFrom::Start(byte_offset)));
                prev_byte != Some(b'\n')
            };
            if mid_line {
                if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                    start = nl + 1;
                } else {
                    // No newline in the window -- the whole window is one
                    // partial line; advance past it so the next window starts
                    // clean.
                    return LogRangeRead {
                        next_offset: byte_offset + n as u64,
                        bytes_total,
                        ..Default::default()
                    };
                }
            }
        }
        // Split the complete lines (start..n), tracking byte offsets.
        let mut lines = Vec::new();
        let mut cur = start;
        let mut line_start_abs = byte_offset + start as u64;
        while cur < buf.len() {
            let nl = buf[cur..].iter().position(|&b| b == b'\n');
            let line_end = match nl {
                Some(p) => cur + p,
                None => break, // last partial line -- drop, the next window reads it whole
            };
            let text = String::from_utf8_lossy(&buf[cur..line_end]).into_owned();
            lines.push((line_start_abs, text));
            line_start_abs += (line_end - cur + 1) as u64; // +1 for the \n
            cur = line_end + 1;
        }
        let next_offset = if lines.is_empty() {
            byte_offset + n as u64
        } else {
            line_start_abs
        };
        LogRangeRead {
            lines,
            next_offset,
            bytes_total,
        }
    }

    fn read_lines_reverse(
        &self,
        session: SessionId,
        from_byte: u64,
        max_bytes: u64,
    ) -> ReverseRead {
        // Backward whole-line read: take the newest max_bytes of the log as one
        // window and split it into lines. A window edge can fall inside a line,
        // so the byte either side of the window is read to learn whether the
        // segments at its ends are lines the log shows whole. The bytes stay
        // raw, so a multi-byte UTF-8 sequence never meets a split inside it.
        let log = self.log_path(session);
        let Ok(mut f) = fs::File::open(&log) else {
            return ReverseRead::default();
        };
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        if size == 0 {
            return ReverseRead::default();
        }
        let from_byte = from_byte.min(size);
        // One byte at the least, so the window's start sits below the read's
        // boundary and a walk over the log always moves down.
        let start = from_byte.saturating_sub(max_bytes.max(1));
        let mut bytes = vec![0u8; (from_byte - start) as usize];
        if f.seek(SeekFrom::Start(start)).is_err() || f.read_exact(&mut bytes).is_err() {
            // The window is lost, not the walk: the batch comes back empty and
            // the resume point below this read carries the walk on.
            bytes.clear();
        }
        let window = LogWindow {
            bytes: &bytes,
            start,
            from_byte,
            covers_log: start == 0 && from_byte == size,
            starts_at_line: start == 0 || byte_ends_line(&mut f, start - 1),
            ends_with_line: from_byte < size && byte_ends_line(&mut f, from_byte),
        };
        let whole = window.whole_lines();
        // Where a walk resumes. A window starting at a line's head left nothing
        // over, so the next read ends there. One starting inside a line holds
        // that line's tail only: the next read must end just past its
        // terminator to hold it whole, or, for a line wider than the read, pass
        // it. The walk always moves down, which is what ends a walk over a log
        // whose lines are wider than the read.
        let next_from = if start == 0 {
            None
        } else if window.starts_at_line {
            Some(start)
        } else {
            let past_line = bytes
                .iter()
                .position(|&b| b == b'\n')
                .map(|i| start + i as u64 + 1)
                .filter(|end| *end < from_byte);
            Some(past_line.unwrap_or(start))
        };
        ReverseRead {
            lines: whole.lines,
            newest_line_returned: whole.newest_line_returned,
            next_from,
        }
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
}
