//! Per-session temp directories for the macOS Seatbelt session. The session
//! owns two: the workspace it fences, and the TMPDIR its commands see.

use houyicoder_context::SandboxError;
use std::env::temp_dir;
use std::fs::create_dir;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};

/// Counter for unique temp-dir names within this process. Shared with the
/// tests, which mint their own fixtures beside a session.
pub(super) static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Longest temp path the session hands out. A build tool puts its Unix socket
/// under TMPDIR, and those paths are capped at 104 bytes, so a long temp path
/// fails every build in the session with an error naming the socket.
///
/// Asserted in the tests rather than enforced at mint time: a host whose temp
/// root is already long cannot be fixed by refusing to start a session.
#[cfg(test)]
pub(super) const MAX_TEMP_PATH_BYTES: usize = 80;

/// Mint a temp directory under the system temp root: the session workspace, and
/// the per-session tmp dir the profile allows and exports as TMPDIR.
///
/// The leaf is short by design (pid in hex plus a counter). Creating it is the
/// collision check, so a name left behind by a crashed process advances the
/// counter instead of failing the session.
pub(super) fn mkdtemp(prefix: &str) -> Result<PathBuf, SandboxError> {
    let base = temp_dir();
    loop {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = base.clone();
        path.push(format!("{prefix}-{}-{n:x}", id()));
        match create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
}
