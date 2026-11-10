//! The cross-process write lock on one memory root.
//!
//! Every process working in the same project writes the same store, so a
//! topic record and its derived index are read, rewritten, and landed under
//! one kernel lock on the root. The lock is exclusive and blocking, and the
//! kernel drops it when the holder exits, so a dead writer never leaves the
//! store locked. A lock that cannot be opened fails the write, since
//! continuing without it would let two processes clobber each other.

use std::fs::File;
use std::fs::OpenOptions;
use std::fs::TryLockError;
use std::fs::create_dir_all;
use std::path::Path;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use houyicoder_context::MemoryError;

use super::MarkdownMemoryProvider;
use super::roots::canonical_identity;

/// The lock file name inside every memory root. A dot name with a non-md
/// suffix, so topic scans never list it as a record.
const LOCK_NAME: &str = ".memory.lock";

/// An exclusive claim on one memory root, released when dropped.
pub(super) struct RootLock {
    /// Held open for as long as the kernel lock is held.
    _file: File,
}

impl RootLock {
    /// Claim a root, waiting for any current holder to release. The root
    /// directory is created first so a first write into a fresh store has a
    /// place for the lock file.
    pub(super) fn acquire(root: &Path) -> Result<Self, MemoryError> {
        let file = open_lock_file(root)?;
        file.lock().map_err(|_| MemoryError::Io)?;
        Ok(Self { _file: file })
    }

    /// Claim a root without waiting, returning None while another writer
    /// holds it. Advisory writes use this: their update is expendable, so a
    /// held root must not stall the caller.
    pub(super) fn try_acquire(root: &Path) -> Result<Option<Self>, MemoryError> {
        let file = open_lock_file(root)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(_)) => Err(MemoryError::Io),
        }
    }
}

impl MarkdownMemoryProvider {
    /// Claim every root, in an order derived from resolved identity, so a
    /// multi-root operation is serialized against every other writer of the
    /// store. Two providers that list the same physical roots in different
    /// order or spelling still claim them in one sequence, which is what
    /// keeps two processes from each holding one root and waiting on the
    /// other. A partial claim is released as the vector drops.
    ///
    /// The roots are created before their identities are resolved. A fresh
    /// store has nothing to canonicalize, and two spellings of one missing
    /// directory would keep distinct literal identities, rank as two roots,
    /// and make the second claim wait on the lock the first claim holds in
    /// this same process.
    pub(super) fn lock_roots(&self) -> Result<Vec<RootLock>, MemoryError> {
        for root in &self.roots {
            create_dir_all(root).map_err(|_| MemoryError::Io)?;
        }
        let mut ordered: Vec<(PathBuf, &Path)> = self
            .roots
            .iter()
            .map(|root| (canonical_identity(root), root.as_path()))
            .collect();
        ordered.sort_by(|a, b| a.0.cmp(&b.0));
        ordered.dedup_by(|a, b| a.0 == b.0);
        ordered
            .into_iter()
            .map(|(_, root)| RootLock::acquire(root))
            .collect()
    }
}

/// Open the root's lock file, creating the root directory and the file when
/// they are missing.
fn open_lock_file(root: &Path) -> Result<File, MemoryError> {
    create_dir_all(root).map_err(|_| MemoryError::Io)?;
    let mut open = OpenOptions::new();
    open.create(true).write(true).truncate(false);
    #[cfg(unix)]
    open.mode(0o600);
    open.open(root.join(LOCK_NAME)).map_err(|_| MemoryError::Io)
}

#[cfg(test)]
mod tests;
