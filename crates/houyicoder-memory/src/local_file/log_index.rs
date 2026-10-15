//! The write state one session holds: which event ids its log already carries,
//! and the offset index maintained beside that log.
//!
//! Both halves are built by a single streaming pass over an existing log and
//! then advanced by each durable append, so they cannot disagree about how much
//! of the log has been accounted for. The pass already happens for the ids,
//! which is what makes the index free to build.
//!
//! The index is derived data, so every write to it is best-effort. A failure
//! latches the index off for the life of the process and is said once through
//! the diagnostic channel; the append that triggered it still succeeds, and a
//! reader with no index to trust walks the log instead.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use houyicoder_context::{EventId, HEADER_LEN, LogIndexEntry, LogIndexHeader, SessionLogEntry};

/// The write state of one session: the ids its log holds, and the offset index
/// published beside it.
pub(super) struct SessionWriteState {
    seen: HashSet<EventId>,
    index: IndexWriter,
}

impl SessionWriteState {
    /// Build the write state for a session by streaming its log once.
    ///
    /// A log that does not exist yet leaves both halves empty, with an index
    /// file holding a header no reader accepts; the first record to land makes
    /// it a real index. A log that does exist is indexed whole, whatever state
    /// its index file was in: the pass reads every record anyway, so rebuilding
    /// costs no more than reusing would, and a rebuild cannot inherit a stale
    /// file.
    ///
    /// A corrupt line is skipped rather than ending the scan, matching how the
    /// id set is built: one bad line must not cost a session its dedup, and it
    /// must not cost it its index either.
    pub(super) fn cold_scan(log: &Path, index_path: PathBuf, interval: u64) -> Self {
        let mut index = IndexWriter::new(index_path, interval);
        index.begin();
        let mut seen = HashSet::new();
        let Ok(file) = File::open(log) else {
            return Self { seen, index };
        };
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
                    // ordinals that put a reader on the wrong record.
                    index.degrade(&format!("reading the log stopped early: {error}"));
                    break;
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
        Self { seen, index }
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
    /// unaffected; readers walk it instead.
    degraded: bool,
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

    /// Start a file: replace whatever it held with a header of zero bytes.
    ///
    /// No magic matches those bytes, so a build interrupted before its header is
    /// written leaves a file that is refused rather than one that looks
    /// complete. The placeholder also holds the space entries are appended
    /// after, which is what lets the header be written last without landing on
    /// top of the first entry.
    fn begin(&mut self) {
        let placeholder = [0u8; HEADER_LEN as usize];
        let written = self.open(true).and_then(|mut f| f.write_all(&placeholder));
        self.report(written, "starting a rebuild");
    }

    /// End a rebuild, taking the byte count the scan consumed and the ordinal the
    /// next record will carry. Reading to the end of a log consumes all of it, so
    /// that count is the length the next append lands at, and a scan that did not
    /// reach the end has already stopped this index being published.
    fn finish(&mut self, covered_len: u64, next_ordinal: u64) {
        self.covered_len = covered_len;
        self.next_ordinal = next_ordinal;
        self.publish_header();
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
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(truncate);
        #[cfg(unix)]
        opts.mode(0o600);
        opts.open(&self.path)
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

    fn degrade(&mut self, reason: &str) {
        if self.degraded {
            return;
        }
        self.degraded = true;
        tracing::warn!(
            index = %self.path.display(),
            reason,
            "the log's offset index is no longer maintained, so reads walk the log instead"
        );
    }
}

#[cfg(test)]
mod tests;
