//! Per-session exclusive file lock (advisory flock) at
//! <sessions_root>/<sid>/session.lock. Guards the hash chain's single-writer
//! invariant when two processes resume the same session id at once (without
//! it, two interactive sessions silently double-write, corrupting the
//! chain). The lock is held for the process
//! lifetime: the guard's Drop releases the flock + closes the fd, so a
//! clean exit or a crash (the OS releases the fd on process death) both
//! free the lock. Advisory (flock) not mandatory: a process that does not
//! call acquire is not blocked, so a legacy writer is not fenced out.
//!
//! Unix-only: flock is a unix primitive. A non-unix build has no
//! SessionLock (the lock is a no-op guard that does not block).

#![cfg(unix)]

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};

/// An held exclusive lock on a session's session.lock file. Drop releases it.
/// The fd is kept open for the guard's lifetime -- the lock lives as long as
/// the guard does.
pub struct SessionLock {
    _lock: Flock<File>,
}

/// A lock acquisition failure: the lock dir could not be created, the lock
/// file could not be opened, or another process holds the lock.
#[derive(Debug)]
pub enum LockError {
    Io(String),
    HeldByOther(String),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(m) => write!(f, "session lock io: {m}"),
            Self::HeldByOther(m) => {
                write!(f, "session held by another live process: {m}")
            }
        }
    }
}

impl std::error::Error for LockError {}

impl SessionLock {
    /// Acquire an exclusive lock on <sessions_root>/<sid>/session.lock,
    /// creating the dir + file if absent. Blocks until the lock is acquired
    /// OR fails with EWOULDBLOCK when another process holds it (the
    /// non-blocking variant is used so a contended resume fails fast with a
    /// message, not a hang).
    pub fn acquire(sid_str: &str, sessions_root: &Path) -> Result<Self, LockError> {
        let dir: PathBuf = sessions_root.join(sid_str);
        std::fs::DirBuilder::new()
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
        // holds the lock -- the resume must refuse + point at fork-session
        // (a new sid gets a new, uncontended lock) or at closing the other
        // window. Flock is RAII: the guard's Drop releases the lock.
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => Ok(Self { _lock: lock }),
            Err((_file, Errno::EAGAIN)) => Err(LockError::HeldByOther(sid_str.to_string())),
            Err((_, e)) => Err(LockError::Io(format!("flock {lock_path:?}: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::SessionId;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_root() -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("session-lock-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A first acquire succeeds + holds; a second acquire on the same sid
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
        std::fs::remove_dir_all(&root).ok();
    }

    /// Different sids do not contend (each has its own lock file).
    #[test]
    fn test_different_sids_dont_contend() {
        let root = temp_root();
        let a = SessionLock::acquire(&SessionId::new().to_string(), &root).expect("acquire a");
        let _b = SessionLock::acquire(&SessionId::new().to_string(), &root).expect("acquire b");
        drop(a);
        std::fs::remove_dir_all(&root).ok();
    }
}
