//! Writing a file through a record that survives a crash.
//!
//! A write that cannot say what the target held is a write nothing can undo,
//! so the previous bytes and the bytes about to land are both stored before
//! the target is touched. A failure at any step before the record is on disk
//! refuses the write; the record is what a recovery reads afterwards.

use std::collections::HashMap;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::fs::{Metadata, metadata, read};
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_context::session_class::{FILE_TRANSACTION_DIR, session_dir};
use houyicoder_context::{ContextBackend, ContextError, SandboxError, SessionId};
use tokio::sync::Mutex as AsyncMutex;

use self::record::{
    CommittedRecord, Identity, ImageRecord, PreparedRecord, write_committed, write_prepared,
};

mod record;
mod recovery;

pub use self::recovery::{RecoveredWrite, RecoveryError, RestoreOutcome, WriteState};

#[cfg(test)]
mod test_support;

/// The most bytes one previous image is kept from. A larger target is still
/// written and its record states that the previous bytes were not kept:
/// refusing instead would take away a write that works today.
pub const MAX_BACKUP_BYTES: u64 = 32 * 1024 * 1024;

/// Distinguishes two writes recorded in one process at the same millisecond.
static WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Records each file write before it happens, so a later recovery can tell
/// what the target held and whether the write landed.
pub struct FileTransaction {
    backend: Arc<dyn ContextBackend>,
    root: Option<PathBuf>,
    backup_ceiling: u64,
    /// One lock per target path. Two writes to the same file run in turn, so
    /// the second records what the first left rather than what both found.
    /// An entry stays for the life of this transaction, so the set is bounded
    /// by the files it wrote. Two transactions over one store do not order
    /// each other: only the writers that take this lock are ordered.
    locks: Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>,
}

impl FileTransaction {
    /// A transaction over the same store the session log uses. Without a store
    /// root, writes pass straight through: there is nowhere for a record to
    /// outlive the process.
    pub fn new(backend: Arc<dyn ContextBackend>) -> Self {
        let root = backend.session_log_root().map(Path::to_path_buf);
        Self {
            backend,
            root,
            backup_ceiling: MAX_BACKUP_BYTES,
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Lower the largest previous image kept, for tests of the limit and for a
    /// caller with its own storage budget.
    pub fn with_backup_ceiling(mut self, bytes: u64) -> Self {
        self.backup_ceiling = bytes;
        self
    }

    /// Write bytes to a workspace path, recording both sides first.
    ///
    /// The prepared record is on disk before the target is touched, so a
    /// refusal leaves the target exactly as it was. A failure to publish the
    /// committed record afterwards does not fail the write: the bytes are
    /// already there, and reporting a failure would have the caller repeat a
    /// write that landed.
    pub async fn write(
        &self,
        session: Option<SessionId>,
        sandbox: &dyn SandboxSession,
        path: &str,
        bytes: Vec<u8>,
    ) -> Result<(), WriteRefusal> {
        let (Some(root), Some(session)) = (self.root.as_deref(), session) else {
            return sandbox
                .write_file(path, bytes)
                .await
                .map_err(|error| WriteRefusal::Write {
                    path: path.to_string(),
                    error,
                });
        };
        let target = sandbox
            .resolve_write(path)
            .map_err(|error| WriteRefusal::Target {
                path: path.to_string(),
                error,
            })?;
        let lock = self.target_lock(&target);
        let _held = lock.lock().await;
        let before = self.capture(&target)?;
        let before_block = match &before {
            Captured::Present {
                bytes: Some(kept), ..
            } => Some(self.store(kept.clone(), path).await?),
            _ => None,
        };
        let after_block = self.store(bytes.clone(), path).await?;
        // The write truncates the file it found, so the identity and the
        // permissions of the image it leaves are the ones it recorded first.
        // A recovery compares them: the same bytes in a file someone replaced
        // or re-permissioned are not the file this write produced.
        let expected_after = ImageRecord {
            present: true,
            block_ref: Some(after_block.clone()),
            bytes: Some(bytes.len() as u64),
            mode: before.mode(),
            identity: before.identity(),
        };
        let operation = operation_name();
        let dir = session_dir(root, session)
            .join(FILE_TRANSACTION_DIR)
            .join(&operation);
        let prepared = PreparedRecord::new(
            &operation,
            &session.to_string(),
            path,
            before.to_record(before_block),
            expected_after.clone(),
        );
        write_prepared(&dir, &prepared).map_err(|error| WriteRefusal::Record {
            path: error.path().to_path_buf(),
            reason: error.reason(),
        })?;
        sandbox
            .write_file(path, bytes)
            .await
            .map_err(|error| WriteRefusal::Write {
                path: path.to_string(),
                error,
            })?;
        let after = match metadata(&target) {
            Ok(meta) => image_of(&meta, Some(after_block)),
            Err(_) => expected_after,
        };
        let committed = CommittedRecord::new(&operation, path, after, now_millis());
        if let Err(error) = write_committed(&dir, &committed) {
            tracing::warn!(
                %error,
                "the write completed but its committed record did not"
            );
        }
        Ok(())
    }

    /// Read what the target holds now. A file that cannot be read is refused
    /// rather than recorded as absent: the write would destroy bytes nothing
    /// kept.
    fn capture(&self, target: &Path) -> Result<Captured, WriteRefusal> {
        let meta = match metadata(target) {
            Ok(meta) => meta,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Captured::Absent),
            Err(error) => {
                return Err(WriteRefusal::Unreadable {
                    path: target.to_path_buf(),
                    error,
                });
            }
        };
        let size = meta.len();
        let bytes = if size > self.backup_ceiling {
            None
        } else {
            Some(read(target).map_err(|error| WriteRefusal::Unreadable {
                path: target.to_path_buf(),
                error,
            })?)
        };
        Ok(Captured::Present {
            bytes,
            size,
            mode: mode_of(&meta),
            identity: identity_of(&meta),
        })
    }

    /// The lock one target's writers take. A poisoned map is not a reason to
    /// skip the lock: another writer panicked while looking a path up, and the
    /// map is still usable.
    fn target_lock(&self, target: &Path) -> Arc<AsyncMutex<()>> {
        let mut locks = match self.locks.lock() {
            Ok(locks) => locks,
            Err(poisoned) => poisoned.into_inner(),
        };
        locks.entry(target.to_path_buf()).or_default().clone()
    }

    /// Store bytes in the block store, returning the hash a record names.
    async fn store(&self, bytes: Vec<u8>, target: &str) -> Result<String, WriteRefusal> {
        let hash = self
            .backend
            .block_put(bytes)
            .await
            .map_err(|error| WriteRefusal::Backup {
                path: target.to_string(),
                error,
            })?;
        Ok(hash.0)
    }
}

/// What the target held when the write began.
enum Captured {
    /// Nothing: the write creates the file.
    Absent,
    /// A file, with its bytes kept only when they fit the ceiling.
    Present {
        bytes: Option<Vec<u8>>,
        size: u64,
        mode: Option<u32>,
        identity: Option<Identity>,
    },
}

impl Captured {
    /// The permissions the target had, when it was there.
    fn mode(&self) -> Option<u32> {
        match self {
            Self::Present { mode, .. } => *mode,
            Self::Absent => None,
        }
    }

    /// What the target was, when it was there.
    fn identity(&self) -> Option<Identity> {
        match self {
            Self::Present { identity, .. } => *identity,
            Self::Absent => None,
        }
    }

    fn to_record(&self, block_ref: Option<String>) -> ImageRecord {
        match self {
            Self::Absent => ImageRecord::absent(),
            Self::Present {
                size,
                mode,
                identity,
                ..
            } => ImageRecord {
                present: true,
                block_ref,
                bytes: Some(*size),
                mode: *mode,
                identity: *identity,
            },
        }
    }
}

/// Why a write did not happen.
#[derive(Debug)]
pub enum WriteRefusal {
    /// The workspace path could not be resolved.
    Target { path: String, error: SandboxError },
    /// The previous contents could not be read, so the write would destroy
    /// bytes nothing recorded.
    Unreadable { path: PathBuf, error: IoError },
    /// The bytes could not be stored, so the record would point at nothing.
    Backup { path: String, error: ContextError },
    /// The prepared record could not be published; the target was not touched.
    Record { path: PathBuf, reason: String },
    /// The write itself failed.
    Write { path: String, error: SandboxError },
}

impl Display for WriteRefusal {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::Target { path, error } => {
                write!(f, "resolve the write target {path}: {error}")
            }
            Self::Unreadable { path, error } => {
                write!(f, "read the previous {}: {error}", path.display())
            }
            Self::Backup { path, error } => {
                write!(f, "keep the previous bytes of {path}: {error}")
            }
            Self::Record { path, reason } => {
                write!(f, "write the record {}: {reason}", path.display())
            }
            Self::Write { path, error } => write!(f, "write {path}: {error}"),
        }
    }
}

/// The record side for a file whose metadata was just read.
fn image_of(meta: &Metadata, block_ref: Option<String>) -> ImageRecord {
    ImageRecord {
        present: true,
        block_ref,
        bytes: Some(meta.len()),
        mode: mode_of(meta),
        identity: identity_of(meta),
    }
}

#[cfg(unix)]
fn mode_of(meta: &Metadata) -> Option<u32> {
    Some(meta.permissions().mode())
}

#[cfg(not(unix))]
fn mode_of(_meta: &Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn identity_of(meta: &Metadata) -> Option<Identity> {
    Some(Identity {
        device: meta.dev(),
        inode: meta.ino(),
    })
}

#[cfg(not(unix))]
fn identity_of(_meta: &Metadata) -> Option<Identity> {
    None
}

/// Names one write: the wall clock first so a listing sorts by time, then the
/// process and a counter so two writes never share a directory.
fn operation_name() -> String {
    let sequence = WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{:013}-{}-{sequence:06}", now_millis(), id())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
