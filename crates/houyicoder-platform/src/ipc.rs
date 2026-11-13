//! Local inter-process byte stream surface: the permission and naming rules
//! for the transport between the host and its clients.
//!
//! Every operating-system difference in that transport stops here, so a caller
//! above this layer binds, connects, and clears a local endpoint through one
//! set of names.

use std::fs::create_dir_all;
use std::io::{Read, Result, Write};
use std::path::Path;

#[cfg(unix)]
use std::fs::{Permissions, set_permissions};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncRead, AsyncWrite};

#[cfg(windows)]
use crate::windows::user::{current_sid_string, system_random};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{
    LocalListener, bind_session, connect_local_streams, remove_endpoint, session_endpoint,
};
#[cfg(windows)]
pub use windows::{
    LocalListener, bind_session, connect_local_streams, remove_endpoint, session_endpoint,
};

#[cfg(test)]
mod tests;

/// The bytes in a pipe-name nonce. A local process that does not hold the
/// recorded endpoint must not be able to guess the name, so the nonce carries
/// the same strength a session id does.
pub const NONCE_LEN: usize = 16;

/// A connected local stream. Each platform has its own stream type, so the
/// listener hands back one erased stream and every caller drives it through
/// the same two async traits.
pub trait LocalStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> LocalStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

/// A connected stream, as the listener hands it back.
pub type LocalConnection = Box<dyn LocalStream>;

/// A connected local stream split into its two directions, for a carrier
/// whose reader blocks on its own thread while the caller writes.
pub struct LocalStreams {
    /// The direction the reader owns.
    pub read: Box<dyn Read + Send>,
    /// The direction the writer owns.
    pub write: Box<dyn Write + Send>,
}

/// Restrict a local listener's file to its owner.
///
/// A Unix domain socket is created with the process default permissions, which
/// on a shared host can leave it open to other users; this sets it to
/// owner-only after the bind. A Windows named pipe carries its access through
/// a security descriptor at creation, so this has nothing to do there.
pub fn set_owner_only(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        set_permissions(path, Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Create a directory only its owner can enter, restricting one that already
/// exists as well.
///
/// The endpoint records and the Unix socket file live here, so a directory
/// another account can enter exposes the endpoint whatever mode the socket
/// itself carries. A Windows directory inherits the access list of the
/// per-user profile it sits in, which is already owner-scoped, so there is no
/// mode to set there.
pub fn create_owner_only_dir(path: &Path) -> Result<()> {
    create_dir_all(path)?;
    #[cfg(unix)]
    {
        set_permissions(path, Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        Ok(())
    }
}

/// The name a session's named pipe is created under. The user scope keeps two
/// accounts apart, the session id lets an attaching client match the name to
/// the session it was given, and the nonce keeps the name from being guessed
/// by a process that never read the recorded endpoint.
pub fn session_pipe_name(scope: &str, session_id: &str, nonce: &[u8]) -> String {
    let nonce = nonce
        .iter()
        .fold(String::with_capacity(nonce.len() * 2), |mut hex, byte| {
            hex.push_str(&format!("{byte:02x}"));
            hex
        });
    format!(r"\\.\pipe\{scope}-{session_id}-{nonce}")
}

/// The security descriptor a session pipe is created with: a protected
/// discretionary list granting general access to the owner alone. A pipe
/// created without one starts from the process default, which grants more
/// than the session channel should carry.
pub fn owner_only_sddl(scope: &str) -> String {
    format!("D:P(A;;GA;;;{scope})")
}

/// The current user's scope string, which is the account's security
/// identifier on Windows. Two accounts never share a value, and it is spelled
/// the same in every system locale.
#[cfg(windows)]
pub(crate) fn current_user_scope() -> Result<String> {
    current_sid_string()
}

/// A fresh nonce for a pipe name, drawn from the platform's random source.
#[cfg(windows)]
pub(crate) fn new_nonce() -> Result<[u8; NONCE_LEN]> {
    system_random()
}
