//! The session log's records: appending one exactly once, and reading them
//! back in the orders the backend exposes.
//!
//! Append dedup and the offset index advance together under one lock, so a
//! record's appearance in the log and its accounting in the index cannot
//! be observed apart. Reads come in two shapes: a strict read that errors on a
//! corrupt line, and a tolerant one that skips and counts, because the
//! snapshot view must survive damage that replay must not hide.

use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::sync::atomic::Ordering;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use houyicoder_context::session_class::session_dir;
use houyicoder_context::{
    ContextError, EventId, LenientRead, SAMPLE_INTERVAL_BYTES, SessionId, SessionLogEntry,
};

use super::LocalFileBackend;
use super::log_index::SessionWriteState;

impl LocalFileBackend {
    /// Build a session's write state by streaming its log line by line
    /// (avoids reading a 500MB log into one heap allocation). Called once
    /// on the first append to a cold session; cached in self.write_state.
    /// Increments read_count so a per-append rescan regression is visible.
    fn load_write_state(&self, session: SessionId) -> SessionWriteState {
        self.read_count.fetch_add(1, Ordering::Relaxed);
        SessionWriteState::cold_scan(
            &self.log_path(session),
            self.index_path(session),
            SAMPLE_INTERVAL_BYTES,
        )
    }

    pub(crate) fn append_sync(&self, event: SessionLogEntry) -> Result<EventId, ContextError> {
        let session = event.session;
        Self::ensure_dir(&session_dir(&self.root, session))?;
        let log = self.log_path(session);
        let id = event.id;
        // Dedup, and the offset index that has to advance with the log. The lock
        // is held through the blocking write so a failure cannot leave the state
        // claiming a record that did not land, and so no second append
        // interleaves between extending the log and accounting for it.
        let mut states = self.write_state.lock().expect("write state poisoned");
        let state = states
            .entry(session)
            .or_insert_with(|| self.load_write_state(session));
        if state.already_written(id) {
            // A no-op extends neither the log nor the index: an entry for a
            // record that never landed puts every later offset out of step.
            return Ok(id);
        }
        let mut json = serde_json::to_string(&event)
            .map_err(|_| ContextError::Corrupt("event failed to serialize".into()))?;
        json.push('\n');
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut f = opts.open(&log).map_err(|_| ContextError::Io)?;
        let mut landed = f.write_all(json.as_bytes());
        // fsync so a crash after the append does not lose the event: the page
        // cache is not durable. The block and checkpoint writes fsync too, so a
        // manifest never references a block that is not on disk.
        if landed.is_ok() {
            landed = f.sync_all();
        }
        if landed.is_err() {
            // A write that reports failure may still have left bytes behind, so
            // the length the index accounts for is no longer the log's. Keep
            // what it published -- those offsets were real -- and stop there
            // rather than compute every later one from a length that is wrong.
            state.log_write_failed();
            return Err(ContextError::Io);
        }
        state.record_durable(id, json.len() as u64);
        Ok(id)
    }

    pub(super) fn read_log(
        &self,
        session: SessionId,
    ) -> Result<Vec<SessionLogEntry>, ContextError> {
        let log = self.log_path(session);
        let content = match fs::read_to_string(&log) {
            Ok(c) => c,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
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

    pub(super) fn read_range_sync(
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

    pub(super) fn replay_sync(
        &self,
        session: SessionId,
    ) -> Result<Vec<SessionLogEntry>, ContextError> {
        self.read_log(session)
    }

    /// Parse what parses, skip and count bad lines. The search snapshot uses
    /// this so one corrupt line does not blank the whole view. The strict
    /// replay path errors on a bad line; two paths, not one shared helper.
    pub(super) fn read_log_lenient_sync(&self, session: SessionId) -> LenientRead {
        let log = self.log_path(session);
        let Ok(f) = File::open(&log) else {
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
}
