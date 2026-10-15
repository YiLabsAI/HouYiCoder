//! The write state one session holds: which event ids its log already carries,
//! and the offset index maintained beside that log.
//!
//! Both halves are built by a single streaming pass over an existing log and
//! then advanced by each durable append, so they cannot disagree about how much
//! of the log has been accounted for. The pass already happens for the ids,
//! which is what makes the index free to build.
//!
//! The index is derived data, so every write to it is best-effort. A failure
//! latches the failing writer off and is said once through the diagnostic
//! channel; the append that triggered it still succeeds, and a reader with no
//! index to trust answers no offset rather than a wrong one. A rebuild pass
//! writes no live file at all: it publishes a whole new file in one move, so a
//! reader concurrent with it sees the previous index or the finished
//! replacement, never one under construction, and a failed pass leaves the
//! previous file and the session's warm state as they were.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use houyicoder_context::{EventId, HEADER_LEN, LogIndexEntry, LogIndexHeader, SessionLogEntry};

/// The write state of one session: the ids its log holds, and the offset index
/// published beside it.
pub(super) struct SessionWriteState {
    seen: HashSet<EventId>,
    index: IndexWriter,
}

/// What one streaming pass over a log left behind.
pub(super) struct ScanOutcome {
    /// The state the pass built: the ids it saw, and the index writer that
    /// owns the index file from here on.
    pub(super) state: SessionWriteState,
    /// Whether the pass read the log to its end. False when the log exists but
    /// could not be opened, or a read stopped midway; the state then holds the
    /// ids the pass saw before it stopped, and its index published nothing. A
    /// caller holding a warm state replaces it only with a completed pass: a
    /// pass that stopped early saw no more of the log than the warm state
    /// already accounted for, and must not cost the ids it holds.
    pub(super) completed: bool,
}

impl SessionWriteState {
    /// Build the write state for a session by streaming its log once, and say
    /// whether the pass reached the log's end.
    ///
    /// A log that does not exist yet counts as read whole: both halves stay
    /// empty, with an index file holding a header no reader accepts; the first
    /// record to land makes it a real index. A log that does exist is indexed
    /// whole, whatever state its index file was in: the pass reads every record
    /// anyway, so rebuilding costs no more than reusing would, and a rebuild
    /// cannot inherit a stale file. Its entries are buffered and the file is
    /// published in one move once the end is reached, so a pass that fails
    /// midway touches nothing on disk and costs no previous index.
    ///
    /// A corrupt line is skipped rather than ending the scan, matching how the
    /// id set is built: one bad line must not cost a session its dedup, and it
    /// must not cost it its index either.
    pub(super) fn scan(log: &Path, index_path: PathBuf, interval: u64) -> ScanOutcome {
        let mut index = IndexWriter::new(index_path, interval);
        let mut seen = HashSet::new();
        let file = match File::open(log) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                index.begin();
                index.live = true;
                return ScanOutcome {
                    state: Self { seen, index },
                    completed: true,
                };
            }
            Err(error) => {
                // The log exists but could not be opened. Publish nothing and
                // touch nothing: an index already on disk was built by a pass
                // that could read the log, which is more than this pass can
                // claim.
                index.degrade(&format!("opening the log failed: {error}"));
                return ScanOutcome {
                    state: Self { seen, index },
                    completed: false,
                };
            }
        };
        index.buffered = Some(Vec::new());
        let mut reader = BufReader::new(file);
        // Raw bytes rather than lines, so a record's offset is exact: an offset
        // off by one points into the middle of a record and reads as corrupt
        // from then on. This also keeps a last record a crash cut short, which
        // carries no terminator, in the byte count.
        let mut raw: Vec<u8> = Vec::new();
        let mut position: u64 = 0;
        let mut ordinal: u64 = 0;
        loop {
            raw.clear();
            let read = match reader.read_until(b'\n', &mut raw) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) => {
                    // The scan stopped short of the log's end, so the ordinal it
                    // would hand the next record is short of the one that record
                    // really has. The ids collected so far still save the appends
                    // they cover; the index publishes nothing rather than
                    // ordinals that put a reader on the wrong record, and the
                    // entries the pass buffered are dropped rather than published.
                    index.degrade(&format!("reading the log stopped early: {error}"));
                    index.buffered = None;
                    return ScanOutcome {
                        state: Self { seen, index },
                        completed: false,
                    };
                }
            };
            let start = position;
            position += read as u64;
            let text = raw.strip_suffix(b"\n".as_slice()).unwrap_or(&raw);
            if text.is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_slice::<SessionLogEntry>(text) else {
                continue;
            };
            seen.insert(event.id);
            index.observe(event.id, ordinal, start);
            ordinal += 1;
        }
        index.finish(position, ordinal);
        index.live = true;
        ScanOutcome {
            state: Self { seen, index },
            completed: true,
        }
    }

    /// The write state for a session, whatever its log could be read to: a log
    /// that refused the pass leaves the ids it yielded and an index that
    /// publishes nothing.
    pub(super) fn cold_scan(log: &Path, index_path: PathBuf, interval: u64) -> Self {
        Self::scan(log, index_path, interval).state
    }

    /// Whether the log already holds this id, so writing it again would
    /// duplicate a record rather than extend the log.
    pub(super) fn already_written(&self, id: EventId) -> bool {
        self.seen.contains(&id)
    }

    /// The log now holds one more record: remember its id, and let the index
    /// publish the offset the record landed at.
    pub(super) fn record_durable(&mut self, id: EventId, written_bytes: u64) {
        self.seen.insert(id);
        self.index.record(id, written_bytes);
    }

    /// A log write reported failure, so the length the index accounts for may no
    /// longer be the log's length. Stop maintaining it: what it already
    /// published described offsets that were real at the time, while every
    /// offset computed from here would be wrong by however many bytes the
    /// failed write left behind.
    pub(super) fn log_write_failed(&mut self) {
        self.index.degrade("a log write reported failure");
    }
}

/// The offset index maintained beside one session's log.
struct IndexWriter {
    path: PathBuf,
    interval: u64,
    /// The identity of the log's first record, which an index built over an
    /// empty log does not have.
    first_event: Option<EventId>,
    /// Bytes of the log accounted for, which is also the offset the next record
    /// will be written at: the log is append-only, and one writer holds it.
    covered_len: u64,
    /// The ordinal the next record will carry.
    next_ordinal: u64,
    /// The offset of the record most recently sampled, or none before the first
    /// one.
    last_sample: Option<u64>,
    /// An index write failed, so this index is no longer maintained. The log is
    /// unaffected; offset queries answer nothing rather than a wrong offset.
    degraded: bool,
    /// Whether this writer maintains a session's index live. False during the
    /// scan pass building it: a failure then published nothing and left the
    /// file on disk alone, which is a different claim than a live writer's
    /// failure, so the one warning says only what the failing case knows.
    live: bool,
    /// The entries a rebuild pass has found so far, held until the pass
    /// completes and the whole file is published in one move. None outside a
    /// pass: a live append writes its entry straight to the published file.
    buffered: Option<Vec<LogIndexEntry>>,
}

impl IndexWriter {
    fn new(path: PathBuf, interval: u64) -> Self {
        Self {
            path,
            // An interval of zero would make every record a sample point and
            // grow the index as fast as the log, which is what the index exists
            // to avoid.
            interval: interval.max(1),
            first_event: None,
            covered_len: 0,
            next_ordinal: 0,
            last_sample: None,
            degraded: false,
            live: false,
            buffered: None,
        }
    }

    /// Account for one record the log now holds durably. The offset is where the
    /// append landed, and an entry for it is published only after the record is
    /// on the log, so an index never points at a byte the log does not hold.
    fn record(&mut self, id: EventId, written_bytes: u64) {
        let offset = self.covered_len;
        let ordinal = self.next_ordinal;
        self.covered_len += written_bytes;
        self.next_ordinal += 1;
        if self.observe(id, ordinal, offset) {
            self.publish_header();
        }
    }

    /// Offer one record to the index: remember it when it is the log's first,
    /// and add an entry when its offset is far enough past the last sample.
    /// Reports whether it added one.
    ///
    /// The header is left alone here, so a scan can add every entry it finds and
    /// write the header once at the end. A header written before the entries it
    /// describes would let a reader trust a file still being built.
    fn observe(&mut self, id: EventId, ordinal: u64, offset: u64) -> bool {
        if self.first_event.is_none() {
            self.first_event = Some(id);
        }
        if self.degraded || !self.sample_due(offset) {
            return false;
        }
        self.last_sample = Some(offset);
        let entry = LogIndexEntry {
            event: id,
            ordinal,
            offset,
        };
        if let Some(buffered) = &mut self.buffered {
            // A rebuild pass holds its entries until the whole file is
            // published in one move, so a pass that fails midway costs the
            // file on disk nothing.
            buffered.push(entry);
            return true;
        }
        let written = self.open(false).and_then(|mut f| {
            f.seek(SeekFrom::End(0))
                .and_then(|_| f.write_all(&entry.encode()))
        });
        self.report(written, "appending an entry");
        true
    }

    /// The first record is always a sample, so an index over a log shorter than
    /// one interval still holds an entry to walk forward from. Later ones follow
    /// the byte interval alone: sampling knows nothing about turn boundaries,
    /// because a reader asks for a byte range and not for a turn.
    fn sample_due(&self, offset: u64) -> bool {
        match self.last_sample {
            None => true,
            Some(previous) => offset.saturating_sub(previous) >= self.interval,
        }
    }

    /// Start the index file of a session whose log does not exist yet: replace
    /// whatever it held with a header of zero bytes.
    ///
    /// No magic matches those bytes, so a build interrupted before its header is
    /// written leaves a file that is refused rather than one that looks
    /// complete. The placeholder also holds the space entries are appended
    /// after, which is what lets the header be written last without landing on
    /// top of the first entry. A pass over a log that does exist never calls
    /// this: it publishes a whole file in one move instead, and a pass that
    /// fails leaves the previous file untouched.
    fn begin(&mut self) {
        let placeholder = [0u8; HEADER_LEN as usize];
        let written = self.open(true).and_then(|mut f| f.write_all(&placeholder));
        self.report(written, "starting an index");
    }

    /// End a rebuild pass, taking the byte count the scan consumed and the
    /// ordinal the next record will carry. Reading to the end of a log consumes
    /// all of it, so that count is the length the next append lands at, and a
    /// scan that did not reach the end has already stopped this index being
    /// published.
    ///
    /// The pass's buffered entries go out with the header in one move: a reader
    /// opening the path while a rebuild runs reads the previous file whole
    /// rather than one between a placeholder and its completion.
    fn finish(&mut self, covered_len: u64, next_ordinal: u64) {
        self.covered_len = covered_len;
        self.next_ordinal = next_ordinal;
        let Some(entries) = self.buffered.take() else {
            self.publish_header();
            return;
        };
        self.publish_whole(&entries);
    }

    /// Write a whole index file, header then every entry, to a fresh file and
    /// move it into place. The move is the publish.
    fn publish_whole(&mut self, entries: &[LogIndexEntry]) {
        if self.degraded {
            return;
        }
        let Some(name) = self.path.file_name() else {
            self.degrade("the index path has no file name");
            return;
        };
        // The temporary file is named per writer, so two rebuilds in one
        // directory cannot truncate each other's half-written bytes before
        // the move.
        static PUBLISH_SEQ: AtomicU64 = AtomicU64::new(0);
        let sequence = PUBLISH_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = self.path.with_file_name(format!(
            "{}.{}.{}.tmp",
            name.to_string_lossy(),
            process::id(),
            sequence
        ));
        let header = LogIndexHeader {
            interval: self.interval,
            first_event: self.first_event,
            covered_len: self.covered_len,
        };
        let published =
            Self::write_whole(&tmp, &header, entries).and_then(|()| fs::rename(&tmp, &self.path));
        if published.is_err() {
            // The move never happened, so the temporary file is debris rather
            // than a publish, and the previous file stays as it was.
            drop(fs::remove_file(&tmp));
        }
        self.report(published, "publishing the rebuilt index");
    }

    fn write_whole(
        tmp: &Path,
        header: &LogIndexHeader,
        entries: &[LogIndexEntry],
    ) -> std::io::Result<()> {
        let mut file = Self::open_at(tmp, true)?;
        file.write_all(&header.encode())?;
        for entry in entries {
            file.write_all(&entry.encode())?;
        }
        file.flush()
    }

    /// Write the header at the start of the file, over the placeholder a rebuild
    /// put there or the header the last sample wrote.
    fn publish_header(&mut self) {
        if self.degraded {
            return;
        }
        let header = LogIndexHeader {
            interval: self.interval,
            first_event: self.first_event,
            covered_len: self.covered_len,
        };
        let written = self.open(false).and_then(|mut f| {
            f.seek(SeekFrom::Start(0))
                .and_then(|_| f.write_all(&header.encode()))
        });
        self.report(written, "writing the header");
    }

    fn open(&self, truncate: bool) -> std::io::Result<File> {
        Self::open_at(&self.path, truncate)
    }

    fn open_at(path: &Path, truncate: bool) -> std::io::Result<File> {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(truncate);
        #[cfg(unix)]
        opts.mode(0o600);
        opts.open(path)
    }

    /// Report an index write that failed by latching the index off. The log is
    /// the source of truth and the index is not, so an append is never failed
    /// over one, and the failure is said once rather than on every later record.
    fn report(&mut self, written: std::io::Result<()>, step: &str) {
        if let Err(error) = written {
            let detail = format!("{step}: {error}");
            self.degrade(&detail);
        }
    }

    /// Latch this writer off and say so once. The warning names only what the
    /// failing case knows: a live writer's failure stops the index being
    /// maintained, while a scan pass that failed published nothing and left
    /// whatever is on disk — and any warm state — as it was.
    fn degrade(&mut self, reason: &str) {
        if self.degraded {
            return;
        }
        self.degraded = true;
        if self.live {
            tracing::warn!(
                index = %self.path.display(),
                reason,
                "the log's offset index is no longer maintained, so offset queries answer nothing"
            );
        } else {
            tracing::warn!(
                index = %self.path.display(),
                reason,
                "this index pass published nothing, so the file on disk stays as it was"
            );
        }
    }
}

#[cfg(test)]
mod tests;
