//! Kernel locks around the snapshot store: one short-held namespace lock and
//! one lease per snapshot directory. The kernel drops a lease when its holder
//! exits. Every probe of a snapshot lease and of another store's namespace
//! lock is non-blocking, so a process that holds one store's lock never waits
//! on a lock held by another path.

#[cfg(test)]
use std::env::temp_dir;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::TryLockError;
use std::fs::create_dir_all;
use std::fs::read_to_string;
use std::fs::remove_file;
use std::fs::rename;
use std::fs::write;
use std::io::Error;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// The lock file that serializes snapshot store metadata.
const STORE_LOCK_NAME: &str = ".snapshots.lock";

/// The path of the namespace lock file inside one store root.
pub(in crate::snapshot) fn namespace_lock_path(store_root: &Path) -> PathBuf {
    store_root.join(STORE_LOCK_NAME)
}

/// The namespace lock over one snapshot store. Dropping it releases the
/// kernel lock. Holders keep the critical section short.
pub(in crate::snapshot) struct StoreLock {
    _lease: SnapshotLease,
}

impl StoreLock {
    /// Wait for the store namespace lock. A lock that cannot be opened or
    /// taken fails the caller, since proceeding without it would let two
    /// processes allocate the same snapshot id.
    pub(in crate::snapshot) fn acquire(store_root: &Path) -> std::io::Result<Self> {
        create_dir_all(store_root)?;
        let file = open_lock_file(&namespace_lock_path(store_root))?;
        file.lock()?;
        Ok(Self {
            _lease: SnapshotLease::from_file(file),
        })
    }
}

/// What a non-blocking claim of a store namespace lock found.
pub(in crate::snapshot) enum StoreProbe {
    /// This process now holds the namespace lock.
    Claimed(StoreLock),
    /// Another process holds the namespace lock.
    Busy,
    /// The lock file exists but cannot be opened or locked.
    Unreadable,
}

/// The lease on one snapshot directory. An undo entry carries its lease, so
/// the protection lasts exactly as long as the entry.
#[derive(Debug)]
pub struct SnapshotLease {
    _file: File,
}

impl SnapshotLease {
    fn from_file(file: File) -> Self {
        Self { _file: file }
    }

    /// Whether the sentinel this lease holds carries the pre-marker tag. The
    /// lease handle does the reading, so a platform that locks the whole
    /// file range for every other handle still lets its owner read the tag.
    pub(in crate::snapshot) fn carries_legacy_tag(&self) -> bool {
        let mut text = String::new();
        let mut file = &self._file;
        file.seek(SeekFrom::Start(0)).is_ok()
            && file
                .read_to_string(&mut text)
                .is_ok_and(|_| text.trim() == LEGACY_TAG)
    }

    /// A lease over a throwaway file, for tests that assemble an undo entry
    /// without a store.
    #[cfg(test)]
    pub(in crate::snapshot) fn test_lease() -> Self {
        static TEST_SEQ: AtomicU64 = AtomicU64::new(0);
        let path = temp_dir().join(format!(
            ".houyicoder-lease-{}-{}",
            id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .expect("test lease file");
        // The name goes at once; the open handle keeps the record alive, and
        // the temporary directory collects no file per entry.
        remove_sidecar(&path);
        file.lock().expect("test lease lock");
        Self { _file: file }
    }
}

/// Distinguishes two tag writes in one process.
static TAG_SEQ: AtomicU64 = AtomicU64::new(0);

/// The text a sentinel carries when a snapshot predates markers and leases.
/// A re-link writes it on the sentinel it creates for such a snapshot, so a
/// later run can tell that state from an empty sentinel beside a directory
/// that has no marker, which means a publish never finished.
pub(in crate::snapshot) const LEGACY_TAG: &str = "legacy";

/// Whether a sentinel records that its snapshot predates markers and leases.
/// This reads through a fresh handle, so it is called before a claim, never
/// while this process holds the lease.
pub(in crate::snapshot) fn is_legacy_sentinel(path: &Path) -> bool {
    read_to_string(path).is_ok_and(|text| text.trim() == LEGACY_TAG)
}

/// Record on a sentinel that its snapshot predates markers and leases. The
/// tag goes to a private name and is renamed into place, so a process that
/// dies during the write leaves either a tagged sentinel or none, never a
/// half-written one. Callers write the tag before claiming the sentinel: a
/// platform that locks the written range for other handles refuses a write
/// through a path while this process holds that lease.
pub(in crate::snapshot) fn mark_legacy_sentinel(path: &Path) -> std::io::Result<()> {
    let temp = path.with_file_name(format!(
        "{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("sentinel"),
        id(),
        TAG_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    write(&temp, LEGACY_TAG)?;
    if let Err(error) = rename(&temp, path) {
        remove_sidecar(&temp);
        return Err(error);
    }
    Ok(())
}

/// Unlink one sidecar file of a snapshot or a staging tree, leaving a real
/// failure in diagnostics for the next sweep.
pub(in crate::snapshot) fn remove_sidecar(path: &Path) {
    if let Err(error) = remove_file(path)
        && error.kind() != ErrorKind::NotFound
    {
        tracing::debug!("snapshot sidecar {} remains: {error}", path.display());
    }
}

/// The sentinel file that records the lease of a published snapshot.
pub(in crate::snapshot) fn sentinel_path(store_root: &Path, snap_name: &str) -> PathBuf {
    store_root.join(format!(".{snap_name}.lease"))
}

/// What the store learned when it tried to claim a lease.
pub(in crate::snapshot) enum LeaseProbe {
    /// This process now holds the lease and may act on the snapshot.
    Claimed(SnapshotLease),
    /// Another process holds the lease.
    Busy,
    /// The sentinel exists but cannot be opened or locked.
    Unreadable,
}

/// Claim a lease without waiting. A missing sentinel is created first, so a
/// snapshot from before leases existed can still be claimed. Whoever holds
/// the lease, or cannot check it, keeps the snapshot out of the caller's
/// hands.
pub(in crate::snapshot) fn claim(path: &Path) -> LeaseProbe {
    let file = match open_lock_file(path) {
        Ok(file) => file,
        Err(_) => return LeaseProbe::Unreadable,
    };
    match file.try_lock() {
        Ok(()) => LeaseProbe::Claimed(SnapshotLease { _file: file }),
        Err(TryLockError::WouldBlock) => LeaseProbe::Busy,
        Err(TryLockError::Error(_)) => LeaseProbe::Unreadable,
    }
}

/// Claim a store's namespace lock without waiting, so a caller can tell an
/// active store from a free one. A lock file that does not exist yet is
/// created. Never waiting matters when the caller already holds another
/// store's lock: the two paths may name the same directory, and a blocking
/// wait would then deadlock against the caller's own handle.
pub(in crate::snapshot) fn try_store_lock(store_root: &Path) -> StoreProbe {
    match claim(&namespace_lock_path(store_root)) {
        LeaseProbe::Claimed(lease) => StoreProbe::Claimed(StoreLock { _lease: lease }),
        LeaseProbe::Busy => StoreProbe::Busy,
        LeaseProbe::Unreadable => StoreProbe::Unreadable,
    }
}

/// Claim a lease that no other process can hold, failing when it is busy or
/// unreadable. Publishing uses this for the name it is about to fill.
pub(in crate::snapshot) fn claim_exclusive(path: &Path) -> std::io::Result<SnapshotLease> {
    match claim(path) {
        LeaseProbe::Claimed(lease) => Ok(lease),
        LeaseProbe::Busy => Err(Error::new(
            ErrorKind::WouldBlock,
            format!("snapshot lease {} is held", path.display()),
        )),
        LeaseProbe::Unreadable => Err(Error::new(
            ErrorKind::PermissionDenied,
            format!("snapshot lease {} cannot be read", path.display()),
        )),
    }
}

/// Open a lock or lease file, creating it and its directory when missing.
/// A namespace lock file stays empty: the kernel lock on the open handle is
/// the whole record. A sentinel may also carry the pre-marker tag, written
/// before the file is opened for its lease.
fn open_lock_file(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let mut open = OpenOptions::new();
    open.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    open.mode(0o600);
    open.open(path)
}
