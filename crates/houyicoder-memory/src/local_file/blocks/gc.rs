//! This module reclaims content-addressed blocks that no live reference names.
//! Marking files every address a live log or checkpoint names into its shard's
//! bucket. A source that cannot be read stops the pass before anything is
//! deleted. A mark left unfinished resumes only while each filed log still
//! holds the size and time the mark recorded; a checkpoint manifest is read on
//! every pass, since rewriting one whole can keep both. A log that grew sends
//! the mark back to the log before it. The sweep resumes after the last shard
//! it finished, and a block inside the write lease survives.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::TryLockError;
use std::io::BufRead;
use std::io::BufReader;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Write;
use std::mem::take;
use std::path::Path;
use std::path::PathBuf;
use std::str::from_utf8;
use std::time::Duration;
use std::time::Instant;
use std::time::UNIX_EPOCH;

use houyicoder_context::BlockHash;
use houyicoder_context::session_class::CHECKPOINT_DIR;
use houyicoder_context::session_class::LOG_FILE;
use houyicoder_protocol::block_ref::block_addresses;
use serde_json::Value;

use self::cursor::CURSOR_SCHEMA;
use self::cursor::Cursor;
use self::cursor::SourceState;
use self::cursor::resumable_cursor;
use self::cursor::source_fingerprint;
use super::SHARD_CHARS;
use crate::local_file::LocalFileBackend;

mod cursor;
mod sweep;

/// The reclaim workspace under the store root, beside the shards.
const GC_DIR: &str = ".gc";
/// The lock serializing two reclaim passes over one store.
const LOCK_NAME: &str = "lock";
/// The last reference source the mark finished.
const CURSOR_NAME: &str = "cursor";
/// The directory holding one bucket per shard prefix.
const REFS_DIR: &str = "refs";
/// Bucket handles held open while one source is filed. A source may name
/// addresses in every shard, and the descriptor limit this pass shares is
/// small on some platforms, so the cache stops well short of it.
const OPEN_BUCKETS: usize = 32;
/// The fewest sources filed between two cursor writes. A write flushes every
/// bucket and rewrites the whole cursor, which holds one entry per filed
/// source, so a fixed interval turns the pass into a quadratic amount of
/// cursor bytes. The interval grows with the sources already filed, and the
/// bytes a pass writes over its longest run then stay linear. Losing the
/// sources since the last write to a crash only means marking them again.
const RECORD_BATCH: usize = 64;
/// The largest record this pass reads from one log line or one manifest. A
/// record past it is treated as unreadable rather than read into memory.
const MAX_RECORD: u64 = 4 * 1024 * 1024;

/// How long a stored block is protected for the writer that has not yet
/// appended the event naming it, and how long one pass may run.
///
/// The write lease must outlast the interval between storing a block and
/// appending the event that references it, across every process that writes
/// to the store. A budget spent while marking ends the pass before any sweep;
/// a budget spent while sweeping keeps the work done so far, and the next
/// pass continues from there.
///
/// One budget covers both phases: a sweep may not begin before the mark
/// finished, and marking is the phase that can run long, so a second budget
/// for the sweep would only lengthen a pass that is already over its time.
/// The budget is checked between units of work, so one source or one shard
/// can run past it: a pass cannot stop inside a source without discarding the
/// work it did on it.
#[derive(Debug, Clone, Copy)]
pub struct ReclaimPolicy {
    pub write_lease: Duration,
    pub budget: Duration,
}

impl ReclaimPolicy {
    /// A policy with the given write lease and budget.
    pub fn new(write_lease: Duration, budget: Duration) -> Self {
        Self {
            write_lease,
            budget,
        }
    }
}

impl Default for ReclaimPolicy {
    fn default() -> Self {
        Self {
            write_lease: Duration::from_secs(60 * 60),
            budget: Duration::from_secs(60),
        }
    }
}

/// What one reclaim pass did, for the caller's log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReclaimReport {
    /// Reference sources read: session logs and checkpoint manifests.
    pub sources: usize,
    /// Stored block files examined. A temporary file a write left behind is
    /// not one of them: it is removed without a reference check.
    pub blocks: usize,
    /// Stored files removed: blocks no reference names, and temp files a write
    /// never renamed.
    pub removed: usize,
    /// Stored files kept because a writer may still be about to name them. A
    /// young temp file a writer is still filling counts here too.
    pub leased: usize,
    /// Stored entries kept because their age could not be read or the removal
    /// failed, as an open handle on another platform makes it fail.
    pub deferred: usize,
    /// Reference values that are not block addresses.
    pub skipped_refs: usize,
    /// Logs ending in a partial append: that event, and so its reference,
    /// never landed.
    pub torn_tail: usize,
    /// False when the budget stopped the sweep early; the remaining shards
    /// are reclaimed by the next pass.
    pub complete: bool,
}

/// Why a reclaim pass stopped.
#[derive(Debug)]
pub enum ReclaimError {
    /// Another process holds the reclaim lock; this pass skipped.
    Busy,
    /// A reference source or a shard could not be read or parsed. A bucket
    /// missing one address would delete a live block, so the pass stops: a
    /// failure while marking deletes nothing, and a failure while sweeping
    /// deletes nothing further.
    Unreadable { source: String, reason: String },
    /// The pass used its budget before marking finished, so nothing was
    /// swept. A sweep that runs out of budget reports an incomplete pass
    /// instead, keeping the work it did.
    Budget,
    /// A reference source appeared or vanished while the mark was reading,
    /// so the sweep would walk a store the mark never saw in full. Nothing
    /// was swept; the next pass reads the new list from the start.
    Sources { reason: String },
    /// The reclaim workspace could not be created or written.
    Scratch { path: String, reason: String },
}

impl Display for ReclaimError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(f, "another pass holds the reclaim lock"),
            Self::Unreadable { source, reason } => {
                write!(f, "reference source {source} could not be read: {reason}")
            }
            Self::Budget => write!(f, "the pass used its budget before marking finished"),
            Self::Sources { reason } => write!(
                f,
                "the reference sources changed while marking ({reason}); nothing was swept"
            ),
            Self::Scratch { path, reason } => {
                write!(f, "reclaim workspace {path} failed: {reason}")
            }
        }
    }
}

impl Error for ReclaimError {}

/// A pass that stopped, with what it freed before stopping. The report is
/// part of the answer rather than left behind: a sweep that fails after some
/// shards freed blocks has already changed the store, and a caller reporting
/// "nothing happened" would be describing a state the store is not in.
#[derive(Debug)]
pub struct ReclaimStop {
    pub error: ReclaimError,
    pub report: ReclaimReport,
}

/// One reference source: a session log or a checkpoint manifest.
enum ReferenceSource {
    Log { key: String, path: PathBuf },
    Checkpoint { key: String, path: PathBuf },
}

impl ReferenceSource {
    fn key(&self) -> &str {
        match self {
            Self::Log { key, .. } | Self::Checkpoint { key, .. } => key,
        }
    }

    fn path(&self) -> &Path {
        match self {
            Self::Log { path, .. } | Self::Checkpoint { path, .. } => path,
        }
    }

    /// Whether this file only gains references. The session log is appended
    /// to, so a longer one still names every address the mark filed from it.
    /// A checkpoint manifest is written whole, so the same length proves
    /// nothing about its contents.
    fn is_append_only(&self) -> bool {
        matches!(self, Self::Log { .. })
    }
}

/// What a re-read of the source list means for a pass that already marked.
enum Relist {
    /// The list is the one the mark read.
    Settled,
    /// The list moved once, so the mark reads the new one.
    Restart,
    /// The list moved again, so the pass stops.
    Moved,
}

/// The decision the re-check makes after one mark. A source that appeared or
/// vanished names blocks no bucket holds, so the sweep must not run under the
/// mark that missed it; a list still moving after a restart ends the pass.
fn relist(changed: bool, restarted: bool) -> Relist {
    match (changed, restarted) {
        (false, _) => Relist::Settled,
        (true, false) => Relist::Restart,
        (true, true) => Relist::Moved,
    }
}

/// The exclusive claim on the reclaim workspace, released when dropped.
struct ReclaimLock {
    /// Held open for as long as the kernel lock is held.
    _file: File,
}

impl ReclaimLock {
    fn acquire(path: &Path) -> Result<Self, ReclaimError> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|e| scratch(path, e))?;
        file.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => ReclaimError::Busy,
            TryLockError::Error(e) => scratch(path, e),
        })?;
        Ok(Self { _file: file })
    }
}

/// The pass's workspace: the buckets being filled, the cursor naming what is
/// done, and the report the caller receives.
struct MarkRun {
    refs: PathBuf,
    cursor: PathBuf,
    fingerprint: String,
    /// The last source filed, when a previous pass left one.
    done: Option<String>,
    /// What each filed source held when it was read, carried across resumes.
    filed: BTreeMap<String, SourceState>,
    /// Bytes filed into each bucket so far, keyed by bucket name.
    buckets: BTreeMap<String, u64>,
    /// The last shard the sweep finished, carried across resumes.
    swept: Option<String>,
    /// Handles held for the source being filed, so a log naming many
    /// addresses in one shard writes them through one handle.
    open: BTreeMap<String, File>,
    /// Sources filed since the cursor was last written.
    since_record: usize,
    /// Sources between two cursor writes, grown after each write.
    record_stride: usize,
    report: ReclaimReport,
}

impl MarkRun {
    /// Open the workspace, resuming a pass a previous one left unfinished.
    fn open(
        workspace: &Path,
        sources: &[ReferenceSource],
        fingerprint: String,
    ) -> Result<Self, ReclaimError> {
        let refs = workspace.join(REFS_DIR);
        LocalFileBackend::ensure_dir(&refs).map_err(|e| scratch(&refs, e))?;
        let cursor = workspace.join(CURSOR_NAME);
        let held = resumable_cursor(&cursor, &refs, sources, &fingerprint);
        let (done, filed, buckets, swept) = match held {
            Some(held) => {
                // The mark counts a bucket's bytes only after a whole line
                // reached it, so a recorded size is a line boundary. Bytes
                // past it belong to the source the crash interrupted, which
                // the mark reads again; keeping them would join a fresh
                // address onto a torn one and lose both.
                for (name, recorded) in &held.buckets {
                    let path = refs.join(name);
                    OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .and_then(|file| file.set_len(*recorded))
                        .map_err(|e| scratch(&path, e))?;
                }
                (Some(held.done), held.filed, held.buckets, held.swept)
            }
            None => {
                // A pass over another source list, or one whose buckets are
                // not the ones it claims, is no pass to resume: the sweep
                // would read missing references as absent ones and free live
                // blocks. Starting the mark over costs one pass over the
                // logs; resuming it costs the store.
                match fs::remove_dir_all(&refs) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(scratch(&refs, e)),
                }
                LocalFileBackend::ensure_dir(&refs).map_err(|e| scratch(&refs, e))?;
                (None, BTreeMap::new(), BTreeMap::new(), None)
            }
        };
        Ok(Self {
            refs,
            cursor,
            fingerprint,
            done,
            record_stride: RECORD_BATCH.max(filed.len()),
            filed,
            buckets,
            swept,
            open: BTreeMap::new(),
            since_record: 0,
            report: ReclaimReport::default(),
        })
    }

    /// File the references of every source the previous pass did not reach.
    fn mark(
        &mut self,
        sources: &[ReferenceSource],
        policy: &ReclaimPolicy,
        started: Instant,
    ) -> Result<(), ReclaimError> {
        for source in sources {
            // Only an append-only source may be skipped on a resume: its
            // recorded size and time still prove the addresses filed from it
            // are there. A manifest is read every pass.
            let already = source.is_append_only()
                && self
                    .done
                    .as_deref()
                    .is_some_and(|done| source.key() <= done);
            if already {
                continue;
            }
            if started.elapsed() >= policy.budget {
                self.flush_buckets()?;
                self.record()?;
                return Err(ReclaimError::Budget);
            }
            // The state is read before the file, so a source that grows while
            // it is being read is recorded as it was before that growth: the
            // next pass sees the difference and reads it again.
            let state = source_state(source.path())
                .ok_or_else(|| unreadable(source.path(), "the source state could not be read"))?;
            match source {
                ReferenceSource::Log { path, .. } => self.mark_log(path)?,
                ReferenceSource::Checkpoint { path, .. } => self.mark_checkpoint(path)?,
            }
            self.report.sources += 1;
            if source.is_append_only() {
                self.filed.insert(source.key().to_string(), state);
                self.done = Some(source.key().to_string());
            }
            self.since_record += 1;
            if self.since_record >= self.record_stride {
                self.flush_buckets()?;
                self.record()?;
            }
        }
        self.flush_buckets()?;
        self.record()?;
        Ok(())
    }

    /// Record how far the pass got. The mark advances only past a source
    /// fully filed: a crash mid-source re-reads it, and a duplicate address in
    /// a bucket costs nothing because the sweep treats one bucket as a set.
    ///
    /// The cursor is written only after the buckets reached the disk, so a
    /// cursor that survives a crash names references the store still holds.
    /// Each write widens the interval to the sources filed so far, so a pass
    /// over many sources writes the cursor a logarithmic number of times.
    fn record(&mut self) -> Result<(), ReclaimError> {
        Cursor {
            schema: CURSOR_SCHEMA,
            done: self.done.clone().unwrap_or_default(),
            fingerprint: self.fingerprint.clone(),
            filed: self.filed.clone(),
            buckets: self.buckets.clone(),
            swept: self.swept.clone(),
        }
        .write_to(&self.cursor)?;
        self.since_record = 0;
        self.record_stride = self.report.sources.max(RECORD_BATCH);
        Ok(())
    }

    /// Record a shard the sweep finished, so a pass stopped by its budget
    /// continues after it.
    fn record_swept(&mut self, shard: &str) -> Result<(), ReclaimError> {
        self.swept = Some(shard.to_string());
        self.record()
    }

    /// Drop a finished pass. The workspace and its lock stay: removing a lock
    /// file another process may have just opened would let two passes sweep
    /// one store at once.
    fn finish(&self) {
        // The cursor goes first. A pass that dies between the two removals
        // then leaves buckets without a cursor, which the next pass reads as
        // a mark to redo. The other order leaves a cursor over buckets that
        // are gone, and a sweep reading each missing bucket as naming nothing
        // would free every block older than the lease.
        match fs::remove_file(&self.cursor) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                tracing::debug!(
                    ?error,
                    "reclaim cursor not cleared; keeping the buckets it describes"
                );
                return;
            }
        }
        if let Err(error) = fs::remove_dir_all(&self.refs) {
            tracing::debug!(?error, "reclaim buckets not cleared");
        }
    }

    /// Push every open bucket to disk and release its handle, so the sizes
    /// the next cursor records are the sizes the store holds.
    fn flush_buckets(&mut self) -> Result<(), ReclaimError> {
        for (name, file) in take(&mut self.open) {
            file.sync_all()
                .map_err(|e| scratch(&self.refs.join(name), e))?;
        }
        Ok(())
    }

    /// The append handle for one bucket, opened on first use.
    fn open_bucket(&mut self, name: &str) -> Result<&mut File, ReclaimError> {
        if !self.open.contains_key(name) && self.open.len() >= OPEN_BUCKETS {
            self.flush_buckets()?;
        }
        let refs = self.refs.clone();
        match self.open.entry(name.to_string()) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                let path = refs.join(name);
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .map_err(|e| scratch(&path, e))?;
                Ok(entry.insert(file))
            }
        }
    }

    /// Stream one log, filing every block address it names.
    fn mark_log(&mut self, path: &Path) -> Result<(), ReclaimError> {
        let file = File::open(path).map_err(|e| unreadable(path, e))?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            // The read stops one byte past the cap, so a log carrying a single
            // unterminated record costs the cap rather than its whole length.
            // The cap counts the separator, which no record of the limit
            // reaches.
            let read = reader
                .by_ref()
                .take(MAX_RECORD + 1)
                .read_until(b'\n', &mut line)
                .map_err(|e| unreadable(path, e))?;
            if read == 0 {
                return Ok(());
            }
            if line.len() as u64 > MAX_RECORD {
                return Err(unreadable(
                    path,
                    "a log record is larger than the pass reads",
                ));
            }
            // A final chunk that does not parse is an append cut off before
            // its last byte, so its event never landed and names nothing. A
            // whole value without its newline is a complete event whose
            // separator did not arrive, so what it names counts.
            let whole = line.ends_with(b"\n");
            let text = match from_utf8(&line) {
                Ok(text) => text,
                Err(_) if !whole => {
                    self.report.torn_tail += 1;
                    return Ok(());
                }
                Err(e) => return Err(unreadable(path, e)),
            };
            // An empty line names nothing, and the log readers skip it too.
            // Treating one as a broken record would stop the pass on a store
            // that is otherwise intact, which is the opposite of what a
            // fail-closed rule is for.
            if text.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(text) {
                Ok(value) => value,
                Err(_) if !whole => {
                    self.report.torn_tail += 1;
                    return Ok(());
                }
                Err(e) => return Err(unreadable(path, e)),
            };
            self.mark_value(&value)?;
        }
    }

    /// File the block addresses one checkpoint manifest names. A manifest
    /// carries none today; the source is scanned so a manifest that starts to
    /// carry one keeps the block without a second edit here.
    fn mark_checkpoint(&mut self, path: &Path) -> Result<(), ReclaimError> {
        let len = fs::metadata(path).map_err(|e| unreadable(path, e))?.len();
        if len > MAX_RECORD {
            return Err(unreadable(path, "a manifest is larger than the pass reads"));
        }
        let content = fs::read_to_string(path).map_err(|e| unreadable(path, e))?;
        let value: Value = serde_json::from_str(&content).map_err(|e| unreadable(path, e))?;
        self.mark_value(&value)
    }

    fn mark_value(&mut self, value: &Value) -> Result<(), ReclaimError> {
        for raw in block_addresses(value) {
            let address = BlockHash(raw.to_string());
            if !address.is_canonical() {
                // Not a block address, so it cannot name a stored block.
                // Counting it keeps the odd value visible without turning a
                // historical format into a failed pass.
                self.report.skipped_refs += 1;
                continue;
            }
            // The bucket is named by the shard prefix, so the sweep reads one
            // bucket for the shard it walks and nothing else.
            let name = &address.0[..SHARD_CHARS];
            let path = self.refs.join(name);
            writeln!(self.open_bucket(name)?, "{}", address.0).map_err(|e| scratch(&path, e))?;
            *self.buckets.entry(name.to_string()).or_default() += address.0.len() as u64 + 1;
        }
        Ok(())
    }
}

impl LocalFileBackend {
    /// Reclaim the blocks no reference names.
    ///
    /// Marking streams every reference source and files what it finds; a
    /// source that cannot be read or parsed stops the pass before anything is
    /// deleted. Sweeping then walks the store one shard at a time and removes
    /// what the mark did not name, except blocks still inside the write lease.
    ///
    /// Returns a stop for a busy lock, an unreadable source, a budget spent
    /// while marking, or a source list that changed under the mark. None of
    /// those four sweeps, though a failure after the sweep began keeps what
    /// the finished shards freed, and the stop carries that report. A budget
    /// spent while sweeping is not a stop: the pass reports an incomplete one
    /// and keeps what it removed. A root that was never written reports an
    /// empty finished pass and creates nothing.
    pub fn reclaim_orphan_blocks(
        &self,
        policy: &ReclaimPolicy,
    ) -> Result<ReclaimReport, ReclaimStop> {
        // A store that was never written holds no blocks, and a maintenance
        // run may be the first thing to look at it. Creating the store here
        // would leave an empty shell behind; the pass has nothing to do. A
        // root that exists and is not a directory is a store this pass cannot
        // read, not an absent one.
        match fs::metadata(&self.root) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(stop(unreadable(
                    &self.root,
                    "the store root is not a directory",
                )));
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Ok(ReclaimReport {
                    complete: true,
                    ..ReclaimReport::default()
                });
            }
            Err(e) => return Err(stop(unreadable(&self.root, e))),
        }
        let workspace = self.cas_dir().join(GC_DIR);
        LocalFileBackend::ensure_dir(&workspace).map_err(|e| stop(scratch(&workspace, e)))?;
        let _lock = ReclaimLock::acquire(&workspace.join(LOCK_NAME)).map_err(stop)?;
        let started = Instant::now();
        let mut sources = self.reference_sources().map_err(stop)?;
        let mut run =
            MarkRun::open(&workspace, &sources, source_fingerprint(&sources)).map_err(stop)?;
        run.mark(&sources, policy, started)
            .map_err(|error| ReclaimStop {
                error,
                report: run.report,
            })?;
        // A source created after the listing names blocks the mark never
        // filed, and the sweep would free them. One restart covers the
        // ordinary case of a session opening during the pass; a second change
        // stops the pass rather than spending the budget marking again.
        let mut restarted = false;
        loop {
            let now = self.reference_sources().map_err(|error| ReclaimStop {
                error,
                report: run.report,
            })?;
            match relist(source_fingerprint(&now) != run.fingerprint, restarted) {
                Relist::Settled => break,
                Relist::Moved => {
                    return Err(ReclaimStop {
                        error: ReclaimError::Sources {
                            reason: "a second source appeared or vanished".to_string(),
                        },
                        report: run.report,
                    });
                }
                Relist::Restart => {
                    restarted = true;
                    sources = now;
                    run = MarkRun::open(&workspace, &sources, source_fingerprint(&sources))
                        .map_err(stop)?;
                    run.mark(&sources, policy, started)
                        .map_err(|error| ReclaimStop {
                            error,
                            report: run.report,
                        })?;
                }
            }
        }
        let report = self
            .sweep(&mut run, policy, started)
            .map_err(|error| ReclaimStop {
                error,
                report: run.report,
            })?;
        // A finished sweep leaves nothing to resume; a partial one keeps its
        // buckets and cursor so the next pass continues where it stopped.
        if report.complete {
            run.finish();
        }
        Ok(report)
    }

    /// The sources a reference can be written to, in a stable order so a
    /// resumed mark knows which ones it already filed.
    ///
    /// Every step stops on an error other than a missing path. A session the
    /// pass cannot name, stat, or open is a session whose references it does
    /// not hold, and sweeping without them would free live blocks.
    fn reference_sources(&self) -> Result<Vec<ReferenceSource>, ReclaimError> {
        let entries = fs::read_dir(&self.root).map_err(|e| unreadable(&self.root, e))?;
        let mut sessions = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| unreadable(&self.root, e))?;
            let path = entry.path();
            // The store root also holds the reclaim workspace and the shards;
            // neither is a session, and neither is stat'ed on the way past.
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                return Err(unreadable(&path, "the session name is not UTF-8"));
            };
            if name.starts_with('.') {
                continue;
            }
            // Only a directory is a session; a stray file at the root names
            // no reference. A path that vanished is not a session either, and
            // any other stat error is a store this pass cannot read.
            match fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => continue,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(unreadable(&path, e)),
            }
            sessions.push((name, path));
        }
        sessions.sort();
        let mut sources = Vec::new();
        for (session, dir) in sessions {
            let log = dir.join(LOG_FILE);
            match fs::metadata(&log) {
                Ok(meta) if meta.is_file() => sources.push(ReferenceSource::Log {
                    key: format!("log:{session}"),
                    path: log,
                }),
                Ok(_) => return Err(unreadable(&log, "the log path is not a file")),
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(unreadable(&log, e)),
            }
            let checkpoints = dir.join(CHECKPOINT_DIR);
            let entries = match fs::read_dir(&checkpoints) {
                Ok(entries) => entries,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(unreadable(&checkpoints, e)),
            };
            let mut files = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|e| unreadable(&checkpoints, e))?;
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    continue;
                }
                match fs::metadata(&path) {
                    Ok(meta) if meta.is_file() => files.push(path),
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(unreadable(&path, e)),
                }
            }
            files.sort();
            for path in files {
                let key = checkpoint_key(&session, &path)?;
                sources.push(ReferenceSource::Checkpoint { key, path });
            }
        }
        sources.sort_by(|a, b| a.key().cmp(b.key()));
        Ok(sources)
    }
}

/// The key one checkpoint manifest files under. A name that is not text
/// cannot key a source: two such manifests would collapse onto one key, and a
/// resumed mark would skip one of them while its references went unfiled.
fn checkpoint_key(session: &str, path: &Path) -> Result<String, ReclaimError> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(unreadable(path, "the checkpoint name is not UTF-8"));
    };
    Ok(format!("cp:{session}/{name}"))
}

/// The size and modification time of a reference source, as the mark sees it.
/// None when the file cannot be stat'ed: a source whose state is unknown
/// cannot be proven unchanged, so the mark stops on it rather than sweeping
/// without the references it holds. A time before the epoch is recorded as the
/// epoch, which is how the sweep reads one too; a source that old is not one a
/// writer is about to name.
fn source_state(path: &Path) -> Option<SourceState> {
    let meta = fs::metadata(path).ok()?;
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |age| age.as_millis() as u64);
    Some(SourceState {
        modified_ms,
        len: meta.len(),
    })
}

fn unreadable(path: &Path, reason: impl Display) -> ReclaimError {
    ReclaimError::Unreadable {
        source: path.display().to_string(),
        reason: reason.to_string(),
    }
}

/// A pass that stopped before its sweep freed anything.
fn stop(error: ReclaimError) -> ReclaimStop {
    ReclaimStop {
        error,
        report: ReclaimReport::default(),
    }
}

fn scratch(path: &Path, reason: impl Display) -> ReclaimError {
    ReclaimError::Scratch {
        path: path.display().to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests;
