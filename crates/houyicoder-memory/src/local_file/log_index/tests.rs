//! Tests for the per-session write state: the offset index maintained beside
//! the log, and the id set that shares its cold pass.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use houyicoder_context::{
    ContextBackend, EventId, LogIndex, SessionEvent, SessionId, SessionLogEntry,
    session_class::{INDEX_FILE, LOG_FILE, session_dir},
};

use super::SessionWriteState;
use crate::local_file::LocalFileBackend;
use std::env::temp_dir;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

fn temp_root() -> PathBuf {
    let dir = temp_dir().join(format!("houyicoder_log_index_test_{}", SessionId::new()));
    fs::create_dir_all(&dir).expect("create the temp root");
    dir
}

fn record(session: SessionId, id: EventId, text: &str) -> SessionLogEntry {
    SessionLogEntry {
        id,
        session,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: text.to_string(),
        },
    }
}

/// The ids of the records a log holds, in the order it holds them. The ordinal
/// an index claims is a position in this sequence, so reading it back is how an
/// ordinal is checked against something other than the writer's own counting.
fn record_ids(log: &Path) -> Vec<EventId> {
    let Ok(file) = File::open(log) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<SessionLogEntry>(&line).ok())
        .map(|entry| entry.id)
        .collect()
}

/// The line a log holds at an offset, read the way a reader reaching for a
/// sampled record reads it.
fn line_at(log: &Path, offset: u64) -> Option<String> {
    let mut reader = BufReader::new(File::open(log).ok()?);
    reader.seek(SeekFrom::Start(offset)).ok()?;
    let mut raw = Vec::new();
    reader.read_until(b'\n', &mut raw).ok()?;
    Some(
        String::from_utf8_lossy(&raw)
            .trim_end_matches('\n')
            .to_string(),
    )
}

/// Check an index against the log it claims to describe, entry by entry.
///
/// Every entry has to name the record actually at its offset and claim the
/// ordinal that record really has, and the header has to name the record the log
/// really starts with. All three are read out of the log rather than out of what
/// the writer said, so an index that agrees with itself and not with the log
/// fails here.
fn assert_entries_hold(index: &LogIndex, log: &Path) {
    let ids = record_ids(log);
    assert_eq!(
        index.header.first_event,
        ids.first().copied(),
        "the header names the record the log starts with"
    );
    for entry in &index.entries {
        let line = line_at(log, entry.offset)
            .unwrap_or_else(|| panic!("no line at offset {}", entry.offset));
        let found: SessionLogEntry = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("offset {} is not a record: {e}", entry.offset));
        assert_eq!(
            found.id, entry.event,
            "the entry names the record at its offset"
        );
        assert_eq!(
            ids.get(entry.ordinal as usize).copied(),
            Some(entry.event),
            "the ordinal is the record's position in the log"
        );
    }
}

/// A log and the index maintained beside it, driven the way the backend drives
/// them: bytes into the log first, then the index told what landed. The
/// interval is a parameter so sampling can be exercised at a few dozen bytes
/// instead of the quarter mebibyte a real log is sampled at.
struct Fixture {
    root: PathBuf,
    log: PathBuf,
    index: PathBuf,
    session: SessionId,
    state: SessionWriteState,
}

impl Fixture {
    fn new(interval: u64) -> Self {
        let root = temp_root();
        let log = root.join(LOG_FILE);
        let index = root.join(INDEX_FILE);
        let state = SessionWriteState::cold_scan(&log, index.clone(), interval);
        Self {
            root,
            log,
            index,
            session: SessionId::new(),
            state,
        }
    }

    /// Put one record in the log and then tell the index it landed, which is the
    /// order the append path does it in: an entry is published for a record only
    /// once the record is durable.
    fn append(&mut self, text: &str) -> EventId {
        let id = EventId::new();
        let mut json =
            serde_json::to_string(&record(self.session, id, text)).expect("serialize the record");
        json.push('\n');
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        let mut file = opts.open(&self.log).expect("open the log");
        file.write_all(json.as_bytes()).expect("write the record");
        self.state.record_durable(id, json.len() as u64);
        id
    }

    /// Throw the in-memory write state away and rebuild it from the files on
    /// disk, which is all a process restart leaves behind.
    fn restart(&mut self, interval: u64) {
        self.state = SessionWriteState::cold_scan(&self.log, self.index.clone(), interval);
    }

    /// The index as a reader would take it: the log's real length and the log's
    /// real first record, neither of them read out of the writer.
    fn load(&self) -> Option<LogIndex> {
        let log_len = fs::metadata(&self.log).ok()?.len();
        let first = record_ids(&self.log).first().copied();
        LogIndex::load(&self.index, log_len, first)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).ok();
    }
}

fn append_once(backend: &LocalFileBackend, session: SessionId, text: &str) -> EventId {
    let id = EventId::new();
    pollster::block_on(backend.append(record(session, id, text))).expect("the append lands");
    id
}

/// The first record a session writes is always a sample, so an index over a log
/// too short to reach the interval still holds an entry to walk forward from.
#[test]
fn test_append_publishes_index() {
    let root = temp_root();
    let session = SessionId::new();
    let backend = LocalFileBackend::new(root.clone());
    let id = append_once(&backend, session, "the first record");
    let dir = session_dir(&root, session);
    assert!(dir.join(INDEX_FILE).is_file(), "an append leaves an index");

    let loaded = LogIndex::load(&dir.join(INDEX_FILE), backend.log_size(session), Some(id))
        .expect("the index an append published");
    assert_eq!(loaded.entries.len(), 1, "the first record is sampled");
    assert_eq!(loaded.entries[0].event, id);
    assert_eq!(loaded.entries[0].ordinal, 0);
    assert_eq!(loaded.entries[0].offset, 0);
    assert_eq!(loaded.header.covered_len, backend.log_size(session));
    assert_entries_hold(&loaded, &dir.join(LOG_FILE));
    fs::remove_dir_all(&root).ok();
}

/// A repeat of a record already in the log extends neither the log nor the
/// index. An entry for a record that was never written would put every later
/// offset out of step with the log, and a covered length past the log's real
/// length is what makes a reader refuse the whole file.
#[test]
fn test_dedup_leaves_index() {
    let root = temp_root();
    let session = SessionId::new();
    let backend = LocalFileBackend::new(root.clone());
    let entry = record(session, EventId::new(), "written once");
    pollster::block_on(backend.append(entry.clone())).expect("the first append lands");
    let log_len = backend.log_size(session);
    pollster::block_on(backend.append(entry.clone())).expect("the repeat is a no-op");

    assert_eq!(backend.log_size(session), log_len, "the log did not grow");
    let dir = session_dir(&root, session);
    let loaded = LogIndex::load(&dir.join(INDEX_FILE), log_len, Some(entry.id))
        .expect("the index the first append published");
    assert_eq!(loaded.entries.len(), 1, "the repeat added no entry");
    assert_eq!(loaded.entries[0].ordinal, 0);
    assert_eq!(
        loaded.header.covered_len, log_len,
        "nor advanced the header"
    );
    fs::remove_dir_all(&root).ok();
}

/// Sampling follows the byte interval: consecutive samples are at least one
/// interval apart, and a log of records far smaller than the interval is
/// sampled far less often than it is written.
#[test]
fn test_sampling_follows_interval() {
    let mut fixture = Fixture::new(400);
    for i in 0..20 {
        fixture.append(&format!("record {i}"));
    }
    let loaded = fixture.load().expect("the index a sampled log leaves");
    assert_eq!(loaded.entries[0].offset, 0, "the first record is sampled");
    assert!(
        loaded.entries.len() < 20,
        "sampling is sparser than writing"
    );
    assert!(
        loaded
            .entries
            .windows(2)
            .all(|step| step[1].offset - step[0].offset >= 400),
        "consecutive samples are at least one interval apart: {:?}",
        loaded.entries
    );
    assert_entries_hold(&loaded, &fixture.log);
}

/// A caller that asks for no interval at all still gets an index a reader
/// accepts. Sampling every record would grow the index as fast as the log, so the
/// writer takes a floor of one byte and publishes that floor in the header rather
/// than the zero it was handed, which matters because a header recording zero is
/// refused as corrupt.
#[test]
fn test_zero_interval_clamped() {
    let mut fixture = Fixture::new(0);
    fixture.append("the first record");
    fixture.append("the second record");

    let loaded = fixture.load().expect("the index a clamped interval leaves");
    assert_eq!(loaded.header.interval, 1);
    assert_entries_hold(&loaded, &fixture.log);
}

/// Every entry an index holds names the record at its offset and claims that
/// record's real ordinal, over a log sampled often enough to have many entries.
#[test]
fn test_offsets_hold_records() {
    let mut fixture = Fixture::new(48);
    for i in 0..40 {
        fixture.append(&format!("record number {i}, widened so it is not tiny"));
    }
    let loaded = fixture.load().expect("the index a finished run leaves");
    assert!(
        loaded.entries.len() > 5,
        "an interval of 48 bytes samples often: {} entries",
        loaded.entries.len()
    );
    assert_eq!(
        loaded.header.interval, 48,
        "the header carries the interval the entries were sampled at"
    );
    assert_entries_hold(&loaded, &fixture.log);
}

/// A second pass over the same log rebuilds the index from the records rather
/// than from the file the first pass left, and lands on the same answer. A
/// process holds no write state across a restart, so this is the path every
/// restart takes.
#[test]
fn test_cold_scan_indexes_log() {
    let mut fixture = Fixture::new(48);
    for i in 0..20 {
        fixture.append(&format!("record number {i}, widened so it is not tiny"));
    }
    let before = fixture.load().expect("the index the first pass left");
    drop(SessionWriteState::cold_scan(
        &fixture.log,
        fixture.index.clone(),
        48,
    ));
    let after = fixture.load().expect("the index the second pass left");
    assert_eq!(before.entries, after.entries, "a rebuild is idempotent");
    assert_eq!(
        before.header, after.header,
        "and reproduces the header whole, not only the entries"
    );
    assert_entries_hold(&after, &fixture.log);
}

/// A process holds no write state across a restart, so a fresh backend over an
/// existing log rebuilds both halves from the records: the index it goes on to
/// maintain describes the log it found, and a record the previous process wrote
/// is still a duplicate rather than something to append again.
#[test]
fn test_restart_rebuilds_state() {
    let root = temp_root();
    let session = SessionId::new();
    let first_backend = LocalFileBackend::new(root.clone());
    let entry = record(session, EventId::new(), "written before the restart");
    pollster::block_on(first_backend.append(entry.clone())).expect("the append lands");
    let first_len = first_backend.log_size(session);
    drop(first_backend);

    let second = LocalFileBackend::new(root.clone());
    pollster::block_on(second.append(entry.clone())).expect("the replay is a no-op");
    assert_eq!(
        second.log_size(session),
        first_len,
        "a restart still knows what the log holds"
    );

    append_once(&second, session, "written after the restart");
    let dir = session_dir(&root, session);
    let loaded = LogIndex::load(
        &dir.join(INDEX_FILE),
        second.log_size(session),
        Some(entry.id),
    )
    .expect("the index a restart rebuilt");
    assert_entries_hold(&loaded, &dir.join(LOG_FILE));
    fs::remove_dir_all(&root).ok();
}

/// A restarted writer picks up at the log's real end rather than at zero, so the
/// entry it publishes for its own record names that record. An interval of one
/// byte makes every record a sample, which is what puts that entry on disk to be
/// checked; the header covers the log exactly because the last append sampled.
#[test]
fn test_restart_continues_index() {
    let mut fixture = Fixture::new(1);
    for i in 0..4 {
        fixture.append(&format!("record {i}"));
    }
    fixture.restart(1);
    let last = fixture.append("the record a restart writes");

    let loaded = fixture.load().expect("the index the restarted writer left");
    let log_len = fs::metadata(&fixture.log).expect("the log exists").len();
    assert_eq!(
        loaded.header.covered_len, log_len,
        "the header covers the whole log"
    );
    assert_eq!(
        loaded.entries.last().map(|entry| entry.event),
        Some(last),
        "the restarted writer sampled the record it appended: {:?}",
        loaded.entries
    );
    assert_entries_hold(&loaded, &fixture.log);
}

/// A line that is not a record costs neither the id set nor the index: the scan
/// goes past it, gives it no ordinal, and the record after it is still the
/// second record. A blank line is the same case with nothing to parse.
#[test]
fn test_corrupt_line_skipped() {
    let root = temp_root();
    let log = root.join(LOG_FILE);
    let session = SessionId::new();
    let first = record(session, EventId::new(), "first");
    let second = record(session, EventId::new(), "second");
    let mut bytes = serde_json::to_string(&first).expect("serialize the first");
    bytes.push('\n');
    bytes.push_str("{ this line is not a record\n");
    bytes.push('\n');
    bytes.push_str(&serde_json::to_string(&second).expect("serialize the second"));
    bytes.push('\n');
    fs::write(&log, bytes).expect("write the log");

    let state = SessionWriteState::cold_scan(&log, root.join(INDEX_FILE), 24);
    assert!(state.already_written(first.id), "the first record is known");
    assert!(
        state.already_written(second.id),
        "so is the one after the garbage"
    );

    let log_len = fs::metadata(&log).expect("the log exists").len();
    let loaded = LogIndex::load(&root.join(INDEX_FILE), log_len, Some(first.id))
        .expect("the index a scan over a partly corrupt log leaves");
    assert_entries_hold(&loaded, &log);
    assert!(
        loaded
            .entries
            .iter()
            .any(|entry| entry.event == second.id && entry.ordinal == 1),
        "the garbage line takes no ordinal: {:?}",
        loaded.entries
    );
    fs::remove_dir_all(&root).ok();
}

/// A log whose last record a crash cut short: the scan gives those bytes no
/// ordinal, keeps the entry it did find, and still counts them, so the offset the
/// next record lands at is the file's real end rather than the end of the last
/// whole record.
#[test]
fn test_torn_tail_counted() {
    let root = temp_root();
    let log = root.join(LOG_FILE);
    let session = SessionId::new();
    let whole = record(session, EventId::new(), "the record that landed");
    let mut bytes = serde_json::to_string(&whole).expect("serialize the record");
    bytes.push('\n');
    bytes.push_str("{\"id\":\"01ARZ3NDEKTSV4RRFFQ69G5FA");
    fs::write(&log, &bytes).expect("write the log");

    let index = root.join(INDEX_FILE);
    let state = SessionWriteState::cold_scan(&log, index.clone(), 24);
    assert!(state.already_written(whole.id), "the whole record is known");

    let log_len = fs::metadata(&log).expect("the log exists").len();
    let loaded = LogIndex::load(&index, log_len, Some(whole.id))
        .expect("the index over a log with a torn tail");
    assert_eq!(
        loaded.header.covered_len, log_len,
        "the torn bytes are counted, so the next offset is the real end"
    );
    assert_entries_hold(&loaded, &log);
    fs::remove_dir_all(&root).ok();
}

/// A log the scan cannot read leaves no index behind, rather than an empty one.
/// Reading stopped short of the log's end, so the ordinal the next record would
/// be given is short of the one it really has, and a header covering none of
/// the log would say the scan had finished. The state the pass leaves keeps
/// publishing nothing, so the ids it did see still dedup the appends they
/// cover.
///
/// A directory answers an open and then refuses a read, which is how a log path
/// becomes unreadable without injecting a fault. Unix only: elsewhere the open
/// itself fails, and the scan refuses the same way before reading anything.
#[cfg(unix)]
#[test]
fn test_unreadable_log_degrades() {
    let root = temp_root();
    let log = root.join(LOG_FILE);
    let index = root.join(INDEX_FILE);
    fs::create_dir_all(&log).expect("put a directory where the log goes");

    let outcome = SessionWriteState::scan(&log, index.clone(), 24);
    assert!(
        !outcome.completed,
        "a log that refuses the pass is not read whole"
    );
    let mut state = outcome.state;
    state.record_durable(EventId::new(), 64);
    assert!(
        !index.exists(),
        "a scan that could not read the log publishes nothing, and the state it leaves publishes nothing after"
    );
    fs::remove_dir_all(&root).ok();
}

/// A rebuild that cannot read the log replaces nothing: the index a previous
/// pass published stays byte for byte, and the warm write state still knows
/// the records that landed. A rebuild used to start by clearing the index
/// file, so a log that went unreadable between two good passes cost the index
/// the earlier pass had built, and the empty state it installed cost the
/// dedup its ids.
///
/// Unix only: a directory answers an open and then refuses a read, which is
/// how a log path becomes unreadable without injecting a fault.
#[cfg(unix)]
#[test]
fn test_blocked_rebuild_keeps_state() {
    let root = temp_root();
    let session = SessionId::new();
    let backend = LocalFileBackend::new(root.clone());
    let entry = record(session, EventId::new(), "written once");
    pollster::block_on(backend.append(entry.clone())).expect("the append lands");
    let dir = session_dir(&root, session);
    let index = dir.join(INDEX_FILE);
    let before = fs::read(&index).expect("the append published an index");
    fs::remove_file(dir.join(LOG_FILE)).expect("remove the log");
    fs::create_dir_all(dir.join(LOG_FILE)).expect("block the path with a directory");

    assert!(
        !backend.rebuild_log_index(session),
        "a log the pass cannot read rebuilds nothing"
    );
    assert_eq!(
        fs::read(&index).expect("the index still exists"),
        before,
        "the refused rebuild left the published index untouched"
    );
    // The warm state survived: dedup still knows the record, and knowing it
    // is what keeps the replay from ever touching the blocked log path.
    pollster::block_on(backend.append(entry.clone())).expect("the repeat stays a no-op");
    fs::remove_dir_all(&root).ok();
}

/// Appends and rebuilds run under one write-state lock, so a rebuild cannot
/// replace the state between an append extending the log and the append
/// accounting for it: no record is lost to dedup, none is duplicated in the
/// log, and the index still answers afterwards. The rebuild loops against the
/// appends rather than running once, so the interleaving is not left to
/// timing luck.
#[test]
fn test_rebuild_concurrent_appends() {
    let root = temp_root();
    let session = SessionId::new();
    let backend = Arc::new(LocalFileBackend::new(root.clone()));
    let entries: Vec<SessionLogEntry> = (0..32)
        .map(|i| record(session, EventId::new(), &format!("record {i}")))
        .collect();
    let done = Arc::new(AtomicBool::new(false));
    thread::scope(|scope| {
        let appender = {
            let backend = Arc::clone(&backend);
            let entries = entries.clone();
            let done = Arc::clone(&done);
            scope.spawn(move || {
                for entry in &entries {
                    pollster::block_on(backend.append(entry.clone())).expect("the append lands");
                }
                done.store(true, Ordering::Release);
            })
        };
        let rebuilder = {
            let backend = Arc::clone(&backend);
            let done = Arc::clone(&done);
            scope.spawn(move || {
                while backend.log_size(session) == 0 && !done.load(Ordering::Acquire) {
                    thread::yield_now();
                }
                loop {
                    backend.rebuild_log_index(session);
                    if done.load(Ordering::Acquire) {
                        break;
                    }
                    thread::yield_now();
                }
            })
        };
        appender.join().expect("the appends finish");
        rebuilder.join().expect("the rebuilds finish");
    });

    let dir = session_dir(&root, session);
    let expected: Vec<EventId> = entries.iter().map(|entry| entry.id).collect();
    assert_eq!(record_ids(&dir.join(LOG_FILE)), expected);
    let loaded = LogIndex::load(
        &dir.join(INDEX_FILE),
        backend.log_size(session),
        Some(expected[0]),
    )
    .expect("the index a hammered rebuild leaves is readable");
    assert_entries_hold(&loaded, &dir.join(LOG_FILE));
    // Dedup survived every replacement: replaying a record the run landed
    // extends nothing.
    let len = backend.log_size(session);
    pollster::block_on(backend.append(entries[0].clone())).expect("the replay is a no-op");
    assert_eq!(backend.log_size(session), len, "the log did not grow");
    fs::remove_dir_all(&root).ok();
}

/// A log write that reports failure stops the index being maintained, because
/// the length it accounts for may no longer be the log's. What it already
/// published stays correct, which is worth more than entries whose offsets are
/// all wrong by however many bytes the failed write left behind.
#[test]
fn test_failed_write_stops_index() {
    let mut fixture = Fixture::new(24);
    fixture.append("the record that lands");
    let published = fixture.load().expect("the index before the failure");
    assert_eq!(published.entries.len(), 1);

    fixture.state.log_write_failed();
    fixture.append("the record whose write failed");
    fixture.append("and every record after it");

    let after = fixture.load().expect("the index is still readable");
    assert_eq!(
        after.entries, published.entries,
        "nothing was published after the failure"
    );
    assert_entries_hold(&after, &fixture.log);
}

/// An index that cannot be written at all costs nothing but speed: the append
/// still lands, and the log still holds exactly the records that were asked
/// for. The index is derived, so it is never a reason to fail a write to the
/// thing it is derived from.
#[test]
fn test_append_survives_bad_index() {
    let root = temp_root();
    let session = SessionId::new();
    let dir = session_dir(&root, session);
    fs::create_dir_all(dir.join(INDEX_FILE)).expect("block the index path with a directory");

    let backend = LocalFileBackend::new(root.clone());
    let id = append_once(&backend, session, "written anyway");
    assert!(backend.log_size(session) > 0, "the record reached the log");
    assert_eq!(record_ids(&dir.join(LOG_FILE)), vec![id]);
    fs::remove_dir_all(&root).ok();
}

/// The backend answers an offset query from the index its own appends
/// maintain: the first record is always a sample, so even a log far shorter
/// than the interval has a floor for every position, and the walk from there
/// is the querying reader's job rather than the backend's.
#[test]
fn test_floor_queries_live_index() {
    let root = temp_root();
    let session = SessionId::new();
    let backend = LocalFileBackend::new(root.clone());
    let first = append_once(&backend, session, "the first record");
    append_once(&backend, session, "the second record");

    let entry = backend
        .log_index_floor(session, 0)
        .expect("a live index answers");
    assert_eq!(entry.event, first);
    assert_eq!(entry.ordinal, 0);
    assert_eq!(entry.offset, 0);
    assert_eq!(
        backend.log_index_floor(session, u64::MAX),
        Some(entry),
        "the last entry covers every position past it"
    );
    fs::remove_dir_all(&root).ok();
}

/// A session with no log has no index to query: the answer is None rather
/// than an error, which is what a querying reader degrades on.
#[test]
fn test_floor_missing_log_none() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    assert_eq!(backend.log_index_floor(SessionId::new(), 0), None);
    fs::remove_dir_all(&root).ok();
}

/// An index file holding something other than an index is refused without
/// being read as one, and the rebuild replaces it from the log: queries
/// degrade while the file is foreign and answer again afterwards.
#[test]
fn test_rebuild_restores_queries() {
    let root = temp_root();
    let session = SessionId::new();
    let backend = LocalFileBackend::new(root.clone());
    let first = append_once(&backend, session, "the first record");
    append_once(&backend, session, "the second record");
    let dir = session_dir(&root, session);
    fs::write(dir.join(INDEX_FILE), b"not an index at all").expect("corrupt the index");

    assert_eq!(backend.log_index_floor(session, 0), None);
    assert!(backend.rebuild_log_index(session), "the rebuild validates");
    let entry = backend
        .log_index_floor(session, 0)
        .expect("the rebuilt index answers");
    assert_eq!(entry.event, first);
    assert_eq!(entry.ordinal, 0);
    fs::remove_dir_all(&root).ok();
}

/// A log written without an index, whether seeded directly or left by a
/// build older than the sidecar, is indexed by the rebuild path: queries
/// answer from the file the streaming pass leaves.
#[test]
fn test_rebuild_indexes_direct_log() {
    let root = temp_root();
    let session = SessionId::new();
    let backend = LocalFileBackend::new(root.clone());
    let dir = session_dir(&root, session);
    fs::create_dir_all(&dir).expect("create the session dir");
    let first = EventId::new();
    let mut lines = String::new();
    for id in [first, EventId::new()] {
        let text =
            serde_json::to_string(&record(session, id, "a record")).expect("serialize the record");
        lines.push_str(&text);
        lines.push('\n');
    }
    fs::write(dir.join(LOG_FILE), lines).expect("write the log");

    assert_eq!(backend.log_index_floor(session, 0), None);
    assert!(backend.rebuild_log_index(session));
    let entry = backend
        .log_index_floor(session, 0)
        .expect("the index answers");
    assert_eq!(entry.event, first);
    assert_eq!(entry.ordinal, 0);
    assert_eq!(entry.offset, 0);
    fs::remove_dir_all(&root).ok();
}

/// The rebuild keeps what the write state exists for: the id set comes out
/// of the same streaming pass as the index, so a record already in the log
/// stays deduped afterwards and a new record extends both halves.
#[test]
fn test_rebuild_keeps_dedup() {
    let root = temp_root();
    let session = SessionId::new();
    let backend = LocalFileBackend::new(root.clone());
    let entry = record(session, EventId::new(), "written once");
    pollster::block_on(backend.append(entry.clone())).expect("the append lands");
    let log_len = backend.log_size(session);
    assert!(backend.rebuild_log_index(session));

    pollster::block_on(backend.append(entry.clone())).expect("the repeat is a no-op");
    assert_eq!(backend.log_size(session), log_len, "the log did not grow");
    append_once(&backend, session, "and a new record");
    assert!(backend.log_size(session) > log_len, "the new record landed");
    let floor = backend
        .log_index_floor(session, 1)
        .expect("the index answers past the first record");
    assert_eq!(
        floor.ordinal, 0,
        "a log shorter than the interval samples once, and the walk covers the rest"
    );
    fs::remove_dir_all(&root).ok();
}
