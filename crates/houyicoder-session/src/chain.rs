//! The durable log's integrity: the hash that links each record to the one
//! before it, and the verification of the links a log already holds.
//!
//! A record's prev_hash is the hash of the previous record's line bytes, so
//! tampering with a record breaks the link the next one carries. Writing a
//! link is the append path's business; verifying one is the resume and import
//! paths'. Both live here, over the same hash, so a writer and a verifier
//! cannot disagree about what a chain is.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::MutexGuard;

use houyicoder_context::{
    ContextBackend, ContextError, PrevHash, ReverseRead, SessionId, SessionLogEntry,
};
use sha2::{Digest, Sha256};

use crate::ChainVerdict;
use crate::log_windows;

/// The last hash written per session, so an append links to the record it
/// follows without reading the log back.
///
/// The cache is per process. A log mutated out of band leaves it stale, which
/// is what the verify path exists for: it reads the log rather than the cache.
#[derive(Default)]
pub(crate) struct LogChain {
    last_hashes: Mutex<HashMap<SessionId, PrevHash>>,
}

impl LogChain {
    /// The hash the next record for a session links to: the hash held for the
    /// record before it, or, on a cold read, the hash of the backend's newest
    /// line. None when the session holds no record yet.
    ///
    /// The cold read hashes the raw bytes of the log's newest line rather than
    /// re-serializing the parsed record: the on-disk bytes stay stable across
    /// a schema change that adds a defaulted field, while a re-serialization
    /// drifts and would link the next record to a hash no reader can reproduce.
    pub(crate) async fn link(
        &self,
        backend: &dyn ContextBackend,
        session: SessionId,
    ) -> Result<Option<PrevHash>, ContextError> {
        if let Some(h) = self.held(session) {
            return Ok(Some(h));
        }
        // Reverse-read budget: the newest durable line is one serialized
        // record. A record larger than 1 MiB would be pathological (a tool
        // result with a huge payload), and the fallback below covers the miss.
        const COLD_REVERSE_BYTES: u64 = 1_048_576;
        let read = backend
            .read_lines_reverse(session, u64::MAX, COLD_REVERSE_BYTES)
            .unwrap_or_else(|e| {
                // A backend with no byte log reports Unsupported and a disk
                // read can fail; the reverse read is an optimization, so
                // either falls back to the replay path rather than failing the
                // hash. Replay surfaces a real IO failure itself.
                tracing::warn!("cold reverse-read fell back to replay: {e}");
                ReverseRead::default()
            });
        // The batch holds whole lines newest first and reports whether it
        // reached the log's newest line: a line no window of the read holds
        // never comes back, and the older line the batch starts with instead
        // is the wrong bytes to link the next record to. Trailing bytes no
        // terminator follows hold no line, so a read that reaches the newest
        // whole line above them reports that line, torn tail or not.
        if read.newest_line_returned
            && let Some((_offset, line)) = read.lines.first()
        {
            let hash = hash_line_bytes(line.as_bytes());
            self.remember(session, hash);
            return Ok(Some(hash));
        }
        let events = backend.replay(session).await?;
        let Some(newest) = events.last() else {
            return Ok(None);
        };
        let hash = hash_event(newest)?;
        self.remember(session, hash);
        Ok(Some(hash))
    }

    /// Remember the hash a record just written for a session carries.
    pub(crate) fn remember(&self, session: SessionId, hash: PrevHash) {
        self.lock().insert(session, hash);
    }

    /// The hash held for a session, or None when it holds none.
    pub(crate) fn held(&self, session: SessionId) -> Option<PrevHash> {
        self.lock().get(&session).copied()
    }

    /// Drop the hash held for a session, so the next link reads the log. A
    /// test forces the cold read this way; nothing else invalidates the cache,
    /// because every write to the log goes through this chain.
    #[cfg(test)]
    pub(crate) fn forget(&self, session: SessionId) {
        self.lock().remove(&session);
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SessionId, PrevHash>> {
        self.last_hashes.lock().expect("last_hashes mutex poisoned")
    }
}

/// SHA-256 of the canonical JSON of a record, its own prev_hash included. The
/// write path hashes this form: within one binary it equals the bytes the log
/// line holds, so a cache primed from it agrees with a read of the log.
pub(crate) fn hash_event(entry: &SessionLogEntry) -> Result<PrevHash, ContextError> {
    let bytes = serde_json::to_vec(entry)
        .map_err(|_| ContextError::Corrupt("event failed to serialize".into()))?;
    Ok(digest(&bytes))
}

/// SHA-256 of raw line bytes (the on-disk line without its trailing newline).
/// The verify path hashes the bytes as read, never re-serializing the parsed
/// record, so a schema change does not break an old log: the chain is
/// byte-stable rather than schema-stable.
pub(crate) fn hash_line_bytes(bytes: &[u8]) -> PrevHash {
    digest(bytes)
}

fn digest(bytes: &[u8]) -> PrevHash {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash: [u8; 32] = hasher
        .finalize()
        .as_slice()
        .try_into()
        .expect("sha256 is 32 bytes");
    PrevHash(hash)
}

/// Verify a disk log's links by hashing the raw line bytes, read through
/// forward windows rather than replayed. A line that does not parse, or a
/// record whose prev_hash is not the line before it, yields Unverified with
/// the index and the reason; the caller reports that and resumes anyway.
///
/// Each window is hashed as it is read and dropped, so verifying a log costs
/// one window of memory however long the log is. A record wider than any read
/// cannot be hashed at all, which leaves the links after it unchecked: that is
/// Unverified too, rather than a chain reported whole on a partial reading.
pub(crate) fn verify_disk(backend: &dyn ContextBackend, session: SessionId) -> ChainVerdict {
    /// The bytes a verification window asks for. Small, because the window
    /// widens only to reach a record that overruns it.
    const VERIFY_WINDOW_BYTES: u64 = 64_000;
    if !backend.supports_log_windows() {
        // No lines to read means no lines to hash, so whatever chain this
        // backend holds cannot be checked here. Unverified, rather than a
        // chain reported whole on no reading at all.
        return ChainVerdict::Unverified {
            at_index: 0,
            reason: "the backend has no log readable a line at a time".into(),
        };
    }
    let held = backend.log_size(session);
    let mut prev: Option<PrevHash> = None;
    let mut index = 0usize;
    let mut offset = 0u64;
    loop {
        let read = log_windows::read_window(backend, session, offset, VERIFY_WINDOW_BYTES);
        if read.bytes_total == 0 && held > 0 {
            // The log holds bytes and this read reached none of them: the
            // backend failed, and a chain nothing could be read for is not a
            // chain that holds.
            return ChainVerdict::Unverified {
                at_index: index,
                reason: "the log could not be read".into(),
            };
        }
        if read.unread_records() > 0 {
            return ChainVerdict::Unverified {
                at_index: index,
                reason: "a record wider than any read window could not be hashed".into(),
            };
        }
        for (_offset, line) in &read.lines {
            // Parse only to read prev_hash: a schema drift with defaulted
            // fields still parses, and the link is checked against the raw
            // line bytes.
            let Ok(entry) = serde_json::from_str::<SessionLogEntry>(line) else {
                return ChainVerdict::Unverified {
                    at_index: index,
                    reason: "line failed to parse".into(),
                };
            };
            if entry.prev_hash != prev {
                return ChainVerdict::Unverified {
                    at_index: index,
                    reason: "prev_hash does not chain to the previous line's bytes".into(),
                };
            }
            prev = Some(hash_line_bytes(line.as_bytes()));
            index += 1;
        }
        if read.next_offset <= offset {
            break;
        }
        offset = read.next_offset;
    }
    ChainVerdict::Verified
}

/// Verify an imported event list's links by re-serializing each record to the
/// compact line bytes the exporting binary hashed. The first record must carry
/// no prev_hash. A serialize failure or a mismatch yields Unverified with the
/// index and the reason, and the caller rebuilds the chain regardless.
pub(crate) fn verify_source(events: &[SessionLogEntry]) -> ChainVerdict {
    let mut prev: Option<PrevHash> = None;
    for (index, entry) in events.iter().enumerate() {
        let Ok(bytes) = serde_json::to_vec(entry) else {
            return ChainVerdict::Unverified {
                at_index: index,
                reason: "event failed to serialize".into(),
            };
        };
        if entry.prev_hash != prev {
            return ChainVerdict::Unverified {
                at_index: index,
                reason: "prev_hash does not chain to the previous event".into(),
            };
        }
        prev = Some(hash_line_bytes(&bytes));
    }
    ChainVerdict::Verified
}
