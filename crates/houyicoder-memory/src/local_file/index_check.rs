//! Checking a stored offset index against the log's own bytes.
//!
//! An index is trusted only when it names the log's first record and claims no
//! byte past the log's length. The identity check reads a bounded leading
//! slice, so a log whose first record sits past the cap makes every index over
//! it refusable rather than wrong.

use std::fs;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use houyicoder_context::{
    ContextBackend, EventId, LogIndexEntry, LogIndexReader, SAMPLE_INTERVAL_BYTES, SessionId,
    SessionLogEntry,
};

use super::LocalFileBackend;
use super::log_index::SessionWriteState;

/// The bytes of a log searched for its first record when an index is checked
/// against it. The width matches the widest line a window read can name: a log
/// whose first record is wider cannot be checked, and its index is refused, so
/// offset queries over that log answer nothing rather than a wrong offset.
const FIRST_RECORD_READ_BYTES: u64 = 1024 * 1024;

/// The first record a log holds, read within a byte cap. Some(None) for a log
/// with no record; None when the log cannot be opened or nothing in the cap
/// parses. The index names the first record the scan found, and a scan skips
/// corrupt lines, so this searches for the first line that parses rather than
/// taking the first line: the two must agree for the check to mean anything.
///
/// The cap makes the check asymmetric with the scan, which reads the whole log
/// and names a first record found past the cap. Every reader refuses such an
/// index the same way, so a rebuild over that log reports refusal rather than
/// claiming an index no reader can open: the refusal is what is observable.
fn first_record_id(log: &Path) -> Option<Option<EventId>> {
    let len = fs::metadata(log).ok()?.len();
    if len == 0 {
        return Some(None);
    }
    let mut file = File::open(log).ok()?;
    let take = usize::try_from(len.min(FIRST_RECORD_READ_BYTES)).ok()?;
    let mut buf = vec![0u8; take];
    file.read_exact(&mut buf).ok()?;
    let parsed = buf
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .find_map(|line| serde_json::from_slice::<SessionLogEntry>(line).ok());
    Some(parsed.map(|entry| entry.id))
}

impl LocalFileBackend {
    /// The log's first record identity, confirmed once per session and reused
    /// after. The lock is a leaf: the bounded read that confirms an identity
    /// runs outside it.
    fn known_first_record(&self, log: &Path, session: SessionId) -> Option<Option<EventId>> {
        if let Some(id) = self
            .first_record_ids
            .lock()
            .expect("first-record cache poisoned")
            .get(&session)
        {
            return Some(Some(*id));
        }
        let first = first_record_id(log)?;
        if let Some(id) = first {
            self.first_record_ids
                .lock()
                .expect("first-record cache poisoned")
                .insert(session, id);
        }
        Some(first)
    }

    /// The index is checked against the log on every open, so an answer here
    /// means a file that names the log's own first record and claims no byte
    /// past the log's length. What it cannot mean is that an entry still
    /// holds: the offset it carries is confirmed against the bytes at it by
    /// the reader that walks, not here. What the check itself reads -- a stat,
    /// the first record identity once per session, the index header, and a
    /// logarithmic count of entries -- is sidecar bookkeeping, bounded apart
    /// from any window budget a caller keeps over the log's content.
    pub(super) fn index_floor(&self, session: SessionId, ordinal: u64) -> Option<LogIndexEntry> {
        let log = self.log_path(session);
        let log_len = fs::metadata(&log).ok()?.len();
        let first = self.known_first_record(&log, session)?;
        let mut reader = LogIndexReader::open(&self.index_path(session), log_len, first)?;
        reader.floor_by_ordinal(ordinal)
    }

    /// One streaming pass under the same lock an append holds, so a rebuild
    /// cannot replace the write state between an append extending the log and
    /// the append accounting for the extension. The pass is the cold scan the
    /// first append to a session already pays; holding the lock through it
    /// keeps the rebuild and the appends in one order. A pass that could not
    /// read the log leaves the warm state in place: it saw no more of the log
    /// than the state the backend already holds, and replacing that would cost
    /// the ids it accounts for.
    ///
    /// The answer is whether queries work after the call, not whether the pass
    /// completed: an index published over a log whose first record sits past
    /// the identity check's bounded read is refused by every reader, and
    /// reporting it as working would silence the one warning that says offset
    /// queries degraded.
    pub(super) fn rebuild_index(&self, session: SessionId) -> bool {
        {
            let mut states = self.write_state.lock().expect("write state poisoned");
            let outcome = SessionWriteState::scan(
                &self.log_path(session),
                self.index_path(session),
                SAMPLE_INTERVAL_BYTES,
            );
            if outcome.completed {
                states.insert(session, outcome.state);
            }
        }
        self.log_size(session) == 0 || self.index_floor(session, 0).is_some()
    }
}
