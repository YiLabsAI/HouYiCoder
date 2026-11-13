//! The owner-only restriction, over a real socket file.

#[cfg(unix)]
use std::env::temp_dir;
#[cfg(unix)]
use std::fs::{create_dir_all, metadata, remove_dir_all};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::net::UnixListener;
#[cfg(unix)]
use std::process::id;

#[cfg(unix)]
use super::set_owner_only;

/// A bound socket is restricted to its owner, so a shared host's other users
/// cannot reach it.
#[cfg(unix)]
#[test]
fn test_socket_restricted_to_owner() {
    let dir = temp_dir().join(format!("houyi-ipc-{}", id()));
    create_dir_all(&dir).expect("a temp dir for the socket");
    let path = dir.join("sock");
    let _listener = UnixListener::bind(&path).expect("bind a socket");
    set_owner_only(&path).expect("restrict the socket");
    let mode = metadata(&path).expect("read the socket").mode() & 0o777;
    assert_eq!(mode, 0o600, "the socket is owner-only: {:o}", mode);
    remove_dir_all(&dir).ok();
}
