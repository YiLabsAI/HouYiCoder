//! The persistence port for session records and its file-backed
//! implementation.
//!
//! The in-memory store is the authority for the live process; a registry is
//! attached only when records must survive across processes. The contract is
//! single-writer-per-session, which the composition root satisfies by keeping
//! one live runner per session.

use std::fs::create_dir_all;
use std::fs::read;
#[cfg(test)]
use std::fs::read_dir;
use std::fs::remove_file;
use std::fs::rename;
use std::fs::write;
use std::path::PathBuf;

use houyicoder_context::SessionId;

use super::record::SessionRecord;
use super::state::LifecycleError;

/// The persistence port for session records. Object-safe so the store holds
/// a single shared dispatcher. The in-memory fast path skips this entirely;
/// an impl is attached when records must survive across processes.
///
/// The contract is single-writer-per-session: the composition root serializes
/// writes to a given session id (one live runner at a time owns a session).
/// A multi-writer impl needs an external lock and is deferred.
pub trait SessionRegistry: Send + Sync {
    /// Persist a record. An existing record for the same session id is
    /// overwritten atomically (a reader never sees a partial file).
    fn register(&self, record: &SessionRecord) -> Result<(), LifecycleError>;

    /// Read a record by session id. None when no record is persisted.
    fn load(&self, session_id: SessionId) -> Result<Option<SessionRecord>, LifecycleError>;

    /// Delete a record by session id. Missing is a no-op (not an error) so
    /// the handoff path can drop a file without a race-check.
    fn remove(&self, session_id: SessionId) -> Result<(), LifecycleError>;
}

/// A file-backed SessionRegistry: one JSON file per session id, named by the
/// ULID display string, under a root directory. Atomic write via tmp + rename
/// so a reconnecting reader never sees a half-written record.
///
/// V1 limitation: single-writer-per-session is assumed. Two processes writing
/// the same session id concurrently race and the last rename wins; a real
/// multi-writer needs a lock file or a single-owner server. The composition
/// root serializes writes today (one runner owns a session at a time), so the
/// assumption holds for v1.
#[derive(Clone)]
pub struct FileSessionRegistry {
    root: PathBuf,
}

impl FileSessionRegistry {
    /// Open a registry rooted at the given directory. The directory is
    /// created (recursively) if missing; existing records are readable as-is.
    pub fn open(root: PathBuf) -> Result<Self, LifecycleError> {
        create_dir_all(&root).map_err(|_| LifecycleError::Io)?;
        Ok(Self { root })
    }

    fn path_for(&self, session_id: SessionId) -> PathBuf {
        self.root.join(format!("{}.json", session_id))
    }

    fn write_record(&self, record: &SessionRecord) -> Result<(), LifecycleError> {
        let path = self.path_for(record.session_id);
        let tmp = self.root.join(format!(".{}.tmp", record.session_id));
        let bytes = serde_json::to_vec(record).map_err(|_| LifecycleError::Io)?;
        write(&tmp, bytes).map_err(|_| LifecycleError::Io)?;
        rename(&tmp, &path).map_err(|_| LifecycleError::Io)?;
        Ok(())
    }

    /// Drop every persisted record under the root. Test-only helper so a
    /// test suite can reset between cases without leaking records across
    /// runs. Production callers never need to wipe a registry this way
    /// (cancel + handoff already remove their files).
    #[cfg(test)]
    pub fn clear(&self) {
        if let Ok(entries) = read_dir(&self.root) {
            for e in entries.flatten() {
                drop(remove_file(e.path()));
            }
        }
    }
}

impl SessionRegistry for FileSessionRegistry {
    fn register(&self, record: &SessionRecord) -> Result<(), LifecycleError> {
        self.write_record(record)
    }

    fn load(&self, session_id: SessionId) -> Result<Option<SessionRecord>, LifecycleError> {
        let path = self.path_for(session_id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = read(&path).map_err(|_| LifecycleError::Io)?;
        let record: SessionRecord =
            serde_json::from_slice(&bytes).map_err(|_| LifecycleError::Io)?;
        Ok(Some(record))
    }

    fn remove(&self, session_id: SessionId) -> Result<(), LifecycleError> {
        let path = self.path_for(session_id);
        if path.exists() {
            remove_file(&path).map_err(|_| LifecycleError::Io)?;
        }
        Ok(())
    }
}
