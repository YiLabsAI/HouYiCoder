//! Local inter-process byte stream surface: the permission and naming rules
//! for the transport between the host and its clients.
//!
//! The binding itself is async and lives in the service layer; this layer owns
//! the security a binding must carry, so a socket is never left at the default
//! permissions the process created it with and a pipe name carries the
//! per-user scoping the host requires.

#[cfg(unix)]
use std::fs::{Permissions, set_permissions};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

#[cfg(test)]
mod tests;

/// Restrict a local listener's file to its owner.
///
/// A Unix domain socket is created with the process's default permissions,
/// which on a shared host can leave it open to other users; this sets it to
/// owner-only after the bind. A Windows named pipe carries its access through
/// a security descriptor at creation, so the restriction is a no-op there.
pub fn set_owner_only(path: &Path) -> std::io::Result<()> {
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
