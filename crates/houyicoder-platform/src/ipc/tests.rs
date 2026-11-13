//! The owner-only restriction over a real socket file and directory, round
//! trips over a bound endpoint, the one-listener rule, and the naming and
//! security descriptor a session pipe carries.

use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all};
use std::io::{Read, Write};
use std::process::id;

#[cfg(unix)]
use std::fs::metadata;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::net::UnixListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{
    LocalListener, NONCE_LEN, connect_local_streams, owner_only_sddl, remove_endpoint,
    session_endpoint, session_pipe_name,
};

#[cfg(unix)]
use super::{create_owner_only_dir, set_owner_only};

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

/// A directory holding endpoint records is restricted to its owner, including
/// one created under the process default first.
#[cfg(unix)]
#[test]
fn test_owner_dir_private() {
    let dir = temp_dir().join(format!("houyi-ipc-dir-{}", id()));
    create_dir_all(&dir).expect("create the dir with the default mode");
    create_owner_only_dir(&dir).expect("restrict the dir");
    let mode = metadata(&dir).expect("read the dir").mode() & 0o777;
    assert_eq!(mode, 0o700, "the sessions dir is owner-only: {mode:o}");
    remove_dir_all(&dir).ok();
}

/// A second listener on one endpoint is refused, so two sessions cannot share
/// a name and a client always reaches the listener that recorded it.
#[tokio::test]
async fn test_second_bind_refused() {
    let dir = temp_dir().join(format!("houyi-ipc-exclusive-{}", id()));
    create_dir_all(&dir).expect("a temp dir for the endpoint");
    let endpoint = session_endpoint(&dir, "exclusive").expect("name the endpoint");
    remove_endpoint(&endpoint).expect("clear a leftover endpoint");
    let first = LocalListener::bind(&endpoint).expect("bind the endpoint once");
    let second = LocalListener::bind(&endpoint);
    assert!(
        second.is_err(),
        "a second listener must not share the endpoint"
    );
    drop(first);
    remove_endpoint(&endpoint).ok();
    remove_dir_all(&dir).ok();
}

/// The three parts of the name stay in order and the nonce is spelled as one
/// hex digit pair per byte, so a client holding the sidecar can rebuild it.
#[test]
fn test_pipe_name_carries_nonce() {
    let name = session_pipe_name("S-1-5-21", "abc", &[0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(name, r"\\.\pipe\S-1-5-21-abc-deadbeef");
}

/// A full-size nonce renders two hex digits per byte: a name that dropped a
/// leading zero would collide with another nonce.
#[test]
fn test_nonce_renders_full_width() {
    let name = session_pipe_name("u", "s", &[0u8; NONCE_LEN]);
    assert!(name.ends_with(&"00".repeat(NONCE_LEN)), "{name}");
}

/// The descriptor grants the owner alone, and it is protected, so an
/// inherited entry cannot widen it.
#[test]
fn test_sddl_grants_owner_only() {
    assert_eq!(owner_only_sddl("S-1-5-21"), "D:P(A;;GA;;;S-1-5-21)");
}

/// A client connects to a bound endpoint and bytes cross in both directions,
/// so the listener and the blocking client agree on one stream. On Unix the
/// same bind proves the endpoint file is owner-only, which is the permission a
/// shared host requires.
#[tokio::test]
async fn test_endpoint_round_trips() {
    let dir = temp_dir().join(format!("houyi-ipc-rt-{}", id()));
    create_dir_all(&dir).expect("a temp dir for the endpoint");
    let endpoint = session_endpoint(&dir, "round-trip").expect("name the endpoint");
    remove_endpoint(&endpoint).expect("clear a leftover endpoint");
    let mut listener = LocalListener::bind(&endpoint).expect("bind the endpoint");
    #[cfg(unix)]
    {
        let mode = metadata(&endpoint).expect("read the socket").mode() & 0o777;
        assert_eq!(mode, 0o600, "a bound endpoint is owner-only: {mode:o}");
    }
    let mut streams = connect_local_streams(&endpoint).expect("connect a client");
    let mut server = listener.accept().await.expect("accept the client");
    streams.write.write_all(b"ping\n").expect("write a frame");
    let mut frame = [0u8; 8];
    let read = server.read(&mut frame).await.expect("read the frame");
    assert_eq!(&frame[..read], b"ping\n", "the listener reads the frame");
    server.write_all(b"pong\n").await.expect("write the reply");
    let reply = streams.read.read(&mut frame).expect("read the reply");
    assert_eq!(&frame[..reply], b"pong\n", "the client reads the reply");
    remove_endpoint(&endpoint).ok();
    remove_dir_all(&dir).ok();
}

/// A second client connects to an endpoint already serving one, and both
/// connections carry bytes on their own stream. The listener makes the next
/// instance available before it waits on the current one, so a client that
/// arrives while another is connected is never refused.
#[tokio::test]
async fn test_listener_accepts_second_client() {
    let dir = temp_dir().join(format!("houyi-ipc-pair-{}", id()));
    create_dir_all(&dir).expect("a temp dir for the endpoint");
    let endpoint = session_endpoint(&dir, "two-clients").expect("name the endpoint");
    remove_endpoint(&endpoint).expect("clear a leftover endpoint");
    let mut listener = LocalListener::bind(&endpoint).expect("bind the endpoint");
    let mut first = connect_local_streams(&endpoint).expect("connect the first client");
    let mut first_server = listener.accept().await.expect("accept the first client");
    let mut second = connect_local_streams(&endpoint).expect("connect the second client");
    let mut second_server = listener.accept().await.expect("accept the second client");

    let mut frame = [0u8; 8];
    first.write.write_all(b"one\n").expect("write on the first");
    let read = first_server.read(&mut frame).await.expect("read the first");
    assert_eq!(
        &frame[..read],
        b"one\n",
        "the first connection is its own stream"
    );
    second
        .write
        .write_all(b"two\n")
        .expect("write on the second");
    let read = second_server
        .read(&mut frame)
        .await
        .expect("read the second");
    assert_eq!(
        &frame[..read],
        b"two\n",
        "the second connection is its own stream"
    );

    drop(first);
    drop(second);
    remove_endpoint(&endpoint).ok();
    remove_dir_all(&dir).ok();
}
