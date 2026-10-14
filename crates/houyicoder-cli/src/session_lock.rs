//! Per-session exclusive file lock (advisory flock) at
//! <sessions_root>/<sid>/session.lock. Guards the hash chain's single-writer
//! invariant when two processes resume the same session id at once. The
//! guard's Drop releases the flock, so a clean exit or a crash both free it.
//! Unix holds a real flock; a non-unix build has no flock primitive, so
//! acquire is a no-op guard and the gap is explicit in the type.

use std::path::{Path, PathBuf};

use houyicoder_context::SessionId;
use houyicoder_context::session_class::session_dir;
#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::fcntl::Flock;
#[cfg(unix)]
use nix::fcntl::FlockArg;
#[cfg(test)]
use std::env::temp_dir;
use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fs::DirBuilder;
use std::fs::File;
use std::fs::OpenOptions;
#[cfg(test)]
use std::fs::create_dir_all;
#[cfg(test)]
use std::fs::remove_dir_all;
#[cfg(test)]
use std::fs::write;
#[cfg(test)]
use std::process::id;

/// An held exclusive lock on a session's session.lock file. Drop releases
/// it. On unix the fd is kept open for the guard's lifetime; on a non-unix
/// build the guard holds nothing because no flock primitive exists.
#[cfg(unix)]
pub struct SessionLock {
    _lock: Flock<File>,
}

/// A no-op lock guard on a platform without flock. Holds nothing; acquire
/// always succeeds.
#[cfg(not(unix))]
pub struct SessionLock;

/// A lock acquisition failure: the lock dir could not be created, the lock
/// file could not be opened, or another process holds the lock.
#[derive(Debug)]
pub enum LockError {
    Io(String),
    HeldByOther(String),
}

impl Display for LockError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(m) => write!(f, "session lock io: {m}"),
            Self::HeldByOther(m) => {
                write!(f, "session held by another live process: {m}")
            }
        }
    }
}

impl Error for LockError {}

#[cfg(unix)]
impl SessionLock {
    /// Acquire an exclusive lock on <sessions_root>/<sid>/session.lock,
    /// creating the dir and file if absent. Fails fast with HeldByOther
    /// when another process holds the lock (non-blocking, so a contended
    /// resume fails with a message, not a hang).
    ///
    /// A sid that parses resolves through the store's own spelling rule, so a
    /// session whose directory is named in the other spelling locks beside its
    /// log; joining the string unexamined would create a second directory for
    /// one session, holding a lock over files no reader looks at.
    pub fn acquire(sid_str: &str, sessions_root: &Path) -> Result<Self, LockError> {
        let dir: PathBuf = match SessionId::from_display_string(sid_str) {
            Some(sid) => session_dir(sessions_root, sid),
            None => sessions_root.join(sid_str),
        };
        DirBuilder::new()
            .recursive(true)
            .create(&dir)
            .map_err(|e| LockError::Io(format!("mkdir {dir:?}: {e}")))?;
        let lock_path = dir.join("session.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| LockError::Io(format!("open {lock_path:?}: {e}")))?;
        // Non-blocking exclusive flock: EAGAIN means another live process
        // holds the lock. Flock is RAII: the guard's Drop releases the lock.
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => Ok(Self { _lock: lock }),
            Err((_file, Errno::EAGAIN)) => Err(LockError::HeldByOther(sid_str.to_string())),
            Err((_, e)) => Err(LockError::Io(format!("flock {lock_path:?}: {e}"))),
        }
    }
}

#[cfg(not(unix))]
impl SessionLock {
    /// No-op acquire: a platform without flock has no exclusive lock to
    /// hold. Always succeeds with a guard that enforces nothing.
    pub fn acquire(_sid_str: &str, _sessions_root: &Path) -> Result<Self, LockError> {
        Ok(Self)
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_root() -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = temp_dir().join(format!("session-lock-{}-{n}", id()));
        create_dir_all(&p).unwrap();
        p
    }

    /// A first acquire succeeds and holds; a second acquire on the same sid
    /// fails (held by other) while the first guard lives. Dropping the first
    /// frees the lock so a third acquire succeeds.
    #[test]
    fn test_second_acquire_fails_recovers() {
        let root = temp_root();
        let sid = SessionId::new();
        let sid_str = sid.to_string();
        let first = SessionLock::acquire(&sid_str, &root).expect("first acquire");
        match SessionLock::acquire(&sid_str, &root) {
            Err(LockError::HeldByOther(_)) => {}
            Err(e) => panic!("second acquire must fail with HeldByOther, got: {e:?}"),
            Ok(_) => panic!("second acquire must fail while the first holds, but succeeded"),
        }
        drop(first);
        let _second = SessionLock::acquire(&sid_str, &root).expect("acquire after drop");
        remove_dir_all(&root).ok();
    }

    /// Different sids do not contend (each has its own lock file).
    #[test]
    fn test_different_sids_dont_contend() {
        let root = temp_root();
        let a = SessionLock::acquire(&SessionId::new().to_string(), &root).expect("acquire a");
        let _b = SessionLock::acquire(&SessionId::new().to_string(), &root).expect("acquire b");
        drop(a);
        remove_dir_all(&root).ok();
    }

    /// A session whose directory is named in the ULID spelling locks beside
    /// its log: joining the sid string would make a second directory for one
    /// session, holding a lock over files no reader opens.
    #[test]
    fn test_acquire_resolves_legacy_dir() {
        let root = temp_root();
        let sid = SessionId::new();
        let legacy = root.join(sid.ulid_name());
        create_dir_all(&legacy).unwrap();
        write(legacy.join("log.jsonl"), "[]").unwrap();

        let _lock = SessionLock::acquire(&sid.to_string(), &root).expect("acquire");
        assert!(
            legacy.join("session.lock").is_file(),
            "the lock lands beside the log"
        );
        assert!(
            !root.join(sid.to_string()).exists(),
            "one session does not get a second directory"
        );
        remove_dir_all(&root).ok();
    }
}
