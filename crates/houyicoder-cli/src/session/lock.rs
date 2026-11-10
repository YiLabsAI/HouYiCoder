//! Per-session exclusive file lock at <sessions_root>/<sid>/session.lock.
//! Guards the hash chain's single-writer invariant when two processes resume
//! the same session id at once; Drop releases it, so a clean exit and a crash
//! free it alike. The standard library maps the lock to flock on unix and to a
//! handle lock on windows, so both platforms enforce it with the same code.
//!
//! The kernel lock says someone holds the session, never who. A guard records
//! the store it stands for, so a caller can tell its own writer from another.

#[cfg(test)]
use std::env::temp_dir;
use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fs::DirBuilder;
use std::fs::File;
use std::fs::OpenOptions;
#[cfg(unix)]
use std::fs::Permissions;
use std::fs::TryLockError;
#[cfg(test)]
use std::fs::create_dir_all;
#[cfg(test)]
use std::fs::remove_dir_all;
#[cfg(test)]
use std::fs::write;
use std::io;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::path::PathBuf;
#[cfg(test)]
use std::process::id;
use std::sync::Mutex;
use std::sync::OnceLock;

use houyicoder_context::SessionDescriptorStore;
use houyicoder_context::SessionId;
use houyicoder_context::session_class::LOCK_FILE;
use houyicoder_context::session_class::session_dir;

use std::collections::HashMap;

/// Open a lock file for locking, creating it when the caller asks. The file is
/// a sentinel whose bytes are never read, so the open never truncates: it can
/// land on a file a live holder owns, and rewriting another process's lock
/// file buys nothing. It opens writable because windows refuses to lock a
/// handle it cannot write through.
///
/// On unix the handle is owner-only, whether the open creates the file or
/// finds one. Otherwise a session lock on a shared sessions root opens for any
/// local user, and one who took it without write access holds the session
/// against its owner. State the mode on every handle rather than only at
/// creation: a file an earlier build left wider stays wider until an open
/// repairs it, and it guards the same session either way.
pub(crate) fn open_lock_file(path: &Path, create: bool) -> io::Result<File> {
    let mut open = OpenOptions::new();
    open.create(create).write(true).truncate(false);
    #[cfg(unix)]
    open.mode(0o600);
    let file = open.open(path)?;
    #[cfg(unix)]
    file.set_permissions(Permissions::from_mode(0o600))?;
    Ok(file)
}

/// An exclusive lock on an open file, released when the guard drops. The file
/// carries no content, so the guard is the descriptor and nothing else: only
/// the kernel lock on the open handle means anything.
pub(crate) struct FileLock {
    /// The locked handle, held open for as long as the lock is held.
    file: File,
}

impl FileLock {
    /// Take the non-blocking exclusive lock on an opened file.
    pub(crate) fn try_acquire(file: File) -> Result<Self, TryLockError> {
        file.try_lock()?;
        Ok(Self { file })
    }

    /// Release the lock, leaving the handle open. Unlocking an unlocked handle
    /// is a no-op, so a caller that already released may still drop the guard.
    fn release(&self) {
        drop(self.file.unlock());
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Release before the close rather than leaving it to the close: the
        // lock belongs to the open file description, and a copy of the handle
        // -- what a child forked from a live process holds until it executes
        // -- keeps that description alive past this guard's own close, so a
        // close-only release would outlive the drop.
        self.release();
    }
}

/// An exclusive lock on a session's session.lock file. Drop releases it and
/// withdraws the session from the process registry.
pub struct SessionLock {
    /// The open handle and its lock, held for as long as the session is held.
    _file: FileLock,
    /// The session directory this guard covers, so Drop withdraws it from the
    /// process registry.
    dir: PathBuf,
}

/// The descriptor store a held lock stands for.
///
/// A store serializes writes to one session under a lock of its own, and that
/// lock is per instance: two stores over one root do not serialize with each
/// other. Two guards naming one identity are therefore one writer path -- a
/// launch's recorder and its own background pass -- while an older launch's
/// guard stands for a different instance whose writes ours would race.
///
/// The token is the store's address. It is compared and never dereferenced, so
/// a guard that outlives the store it was bound to leaves a value that is only
/// ever checked for equality, and a later store at the same address could match
/// it. The release order is what rules that out: a host joins the writers of
/// its store before the guard drops, so no writer bound to that store outlives
/// its guard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StoreIdentity(usize);

impl StoreIdentity {
    /// The identity of a store's in-process write lock.
    pub fn of(store: &dyn SessionDescriptorStore) -> Self {
        Self(store as *const dyn SessionDescriptorStore as *const () as usize)
    }

    /// A guard whose store is not built yet. No caller returns this identity,
    /// so an unbound guard is never mistaken for a caller's own writer.
    const UNBOUND: Self = Self(0);
}

/// A lock acquisition failure: the sid names no session, the lock dir could
/// not be created, the lock file could not be opened, or something holds the
/// lock.
#[derive(Debug)]
pub enum LockError {
    InvalidId(String),
    Io(String),
    Held(String),
}

impl Display for LockError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidId(m) => write!(f, "not a session id: {m}"),
            Self::Io(m) => write!(f, "session lock io: {m}"),
            Self::Held(m) => write!(f, "session lock is held: {m}"),
        }
    }
}

impl Error for LockError {}

impl SessionLock {
    /// Acquire an exclusive lock on <sessions_root>/<sid>/session.lock,
    /// creating the dir and file if absent. Fails fast with Held when another
    /// holder has the lock (non-blocking, so a contended resume fails with a
    /// message, not a hang).
    ///
    /// A parsing sid resolves through the store's own spelling rule, so a
    /// session whose directory is named in the other spelling locks beside its
    /// log; joining the string unexamined would create a second directory for
    /// one session and hold a lock over files no reader looks at. A string
    /// that parses as no session id is refused for the same reason: it could
    /// name a path outside the sessions root.
    ///
    /// The guard starts unbound, because admission takes the lock before the
    /// bundle that owns the store exists; bind records the store once it does.
    pub fn acquire(sid_str: &str, sessions_root: &Path) -> Result<Self, LockError> {
        let dir = resolve_dir(sid_str, sessions_root)
            .ok_or_else(|| LockError::InvalidId(sid_str.to_string()))?;
        DirBuilder::new()
            .recursive(true)
            .create(&dir)
            .map_err(|e| LockError::Io(format!("mkdir {dir:?}: {e}")))?;
        let lock_path = dir.join(LOCK_FILE);
        let file = open_lock_file(&lock_path, true)
            .map_err(|e| LockError::Io(format!("open {lock_path:?}: {e}")))?;
        // Non-blocking: WouldBlock means another holder has the lock.
        let file = FileLock::try_acquire(file).map_err(|e| match e {
            TryLockError::WouldBlock => LockError::Held(sid_str.to_string()),
            TryLockError::Error(e) => LockError::Io(format!("lock {lock_path:?}: {e}")),
        })?;
        with_held(|held| held.insert(dir.clone(), StoreIdentity::UNBOUND));
        Ok(Self { _file: file, dir })
    }

    /// Record which store this guard serializes with.
    pub fn bind(&self, store: StoreIdentity) {
        with_held(|held| {
            if let Some(holder) = held.get_mut(&self.dir) {
                *holder = store;
            }
        });
    }

    /// Whether this process holds the session's lock for the store with this
    /// identity. A holder bound to another store serializes with none of our
    /// writes and is answered as foreign.
    pub fn held_by(store: StoreIdentity, sid_str: &str, sessions_root: &Path) -> bool {
        let Some(dir) = resolve_dir(sid_str, sessions_root) else {
            return false;
        };
        with_held(|held| held.get(&dir) == Some(&store))
    }

    /// Whether this process holds the session's lock for any store. A refusal
    /// that lands here is this process's own doing -- a host still closing, or
    /// a background pass still scanning -- so the caller reports it as
    /// temporary rather than as a competitor to attach to.
    pub fn held_in_this_process(sid_str: &str, sessions_root: &Path) -> bool {
        let Some(dir) = resolve_dir(sid_str, sessions_root) else {
            return false;
        };
        with_held(|held| held.contains_key(&dir))
    }
}

/// The session directory a sid locks, resolved through the store's spelling
/// rule. None for a string that names no session.
fn resolve_dir(sid_str: &str, sessions_root: &Path) -> Option<PathBuf> {
    SessionId::from_display_string(sid_str).map(|sid| session_dir(sessions_root, sid))
}

/// Probe an existing lock file without creating it: take the lock and drop
/// the guard immediately. Some means nothing holds the file. None means held
/// or unopenable, and both count as held, since an ambiguous lock state must
/// never lead a caller to treat the file as free.
///
/// The probe walks session directories, so it never creates a missing file.
/// A missing file already means no holder, because taking a lock creates it
/// first.
pub(crate) fn probe_session_lock(lock_path: &Path) -> Option<FileLock> {
    let file = open_lock_file(lock_path, false).ok()?;
    FileLock::try_acquire(file).ok()
}

/// What one probe of a session's lock finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockProbe {
    /// No process holds the session.
    Free,
    /// Another process holds the session, so opening it now would be
    /// refused.
    Held,
    /// The lock file could not be read, so its state is unknown and counted
    /// as taken. This names no holder the user can act on, which is why it
    /// is reported apart from a held lock.
    Unreadable,
}

/// The state of a session's lock right now. A session with no lock file has
/// never been opened by a holder, so absence reads as free; nothing in the
/// product deletes a held lock file, and a hand deletion falls back to the
/// same answer a fresh session gives.
///
/// This answers a moment, not a reservation. The caller still takes the lock
/// for real before it writes, so a holder that arrives in between is refused
/// there.
pub(crate) fn probe_lock_state(sid: SessionId, sessions_root: &Path) -> LockProbe {
    let path = session_dir(sessions_root, sid).join(LOCK_FILE);
    match open_lock_file(&path, false) {
        Ok(file) => lock_probe(FileLock::try_acquire(file).map(|_| ())),
        Err(e) if e.kind() == ErrorKind::NotFound => LockProbe::Free,
        Err(_) => LockProbe::Unreadable,
    }
}

/// The probe's answer for one lock attempt. A would-block means another
/// holder. Any other lock error says the state is unknown, which is neither
/// free nor a holder the user could go looking for.
fn lock_probe(attempt: Result<(), TryLockError>) -> LockProbe {
    match attempt {
        Ok(()) => LockProbe::Free,
        Err(TryLockError::WouldBlock) => LockProbe::Held,
        Err(TryLockError::Error(_)) => LockProbe::Unreadable,
    }
}

/// The session directories this process holds, with the store each stands
/// for. A directory appears once: a second guard for it never exists, because
/// the kernel refuses the open.
///
/// The kernel lock cannot say who holds it, and this is the only record of
/// the locks this process took itself.
fn held_sessions() -> &'static Mutex<HashMap<PathBuf, StoreIdentity>> {
    static HELD: OnceLock<Mutex<HashMap<PathBuf, StoreIdentity>>> = OnceLock::new();
    HELD.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Run one operation over the registry. A panic while the lock was held leaves
/// the map poisoned but its entries still true, so the guard is recovered
/// rather than dropped.
fn with_held<T>(operate: impl FnOnce(&mut HashMap<PathBuf, StoreIdentity>) -> T) -> T {
    let mut held = match held_sessions().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    operate(&mut held)
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        // Release before withdrawing the registry entry: a caller refused in
        // between must find the guard recorded, or it reads its own release as
        // another process's and tells the user to attach to a host that is
        // already going away. The opposite window costs at most a transient
        // answer: a probe that arrives after the release still sees the entry,
        // reads the refusal as this process's own, and retries. No write can
        // land in it, because the host joins this store's writers before the
        // guard drops.
        self._file.release();
        with_held(|held| held.remove(&self.dir));
    }
}

#[cfg(test)]
mod tests;
