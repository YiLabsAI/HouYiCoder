//! Tests for this module.

use super::*;
use crate::lifecycle::SessionLeaseStore;
use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all, remove_file};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::id;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::sleep;

/// The longest path a Unix-domain socket can be bound to, including the
/// trailing NUL: 104 on macOS and 108 on Linux. The shorter figure is used on
/// both so a test that passes on one platform cannot fail on the other.
const SUN_PATH_MAX: usize = 104;

/// A short directory for a socket fixture.
///
/// A socket path has a hard byte limit, and the system temp root can be long:
/// on macOS it is already 71 bytes under a session scratch dir, which leaves
/// well under 40 for the rest. Naming the fixture with a short leaf and
/// asserting the final path keeps the failure readable here rather than at
/// the bind, where the error names the socket and not the directory that made
/// it too long.
fn socket_dir(prefix: &str) -> PathBuf {
    let dir = temp_dir().join(format!("{prefix}-{}", id()));
    create_dir_all(&dir).ok();
    dir
}

/// The socket path inside a fixture directory, checked against the platform
/// limit before it is used. A path that cannot be bound is a fault in the
/// fixture, not a property of the code under test.
fn socket_path(dir: &Path, leaf: &str) -> PathBuf {
    let path = dir.join(leaf);
    let len = path.as_os_str().len();
    assert!(
        len < SUN_PATH_MAX,
        "fixture socket path is {len} bytes, over the {SUN_PATH_MAX}-byte \
         limit; shorten the leaf or the directory name: {}",
        path.display()
    );
    path
}

/// A frame round-trips through the carrier: writing on one stream end
/// lands on the FrameCarrier inbound channel, and sending on the outbound
/// channel lands on the other stream end. Proves the byte path without a
/// host or runner.
#[tokio::test]
async fn test_bridge_round_trips_frame() {
    let (a, b) = StdUnixStream::pair().expect("pair");
    a.set_nonblocking(true).ok();
    b.set_nonblocking(true).ok();
    let server_stream = UnixStream::from_std(a).expect("from_std a");
    let mut client_stream = UnixStream::from_std(b).expect("from_std b");

    let mut io = bridge_uds(server_stream).await;

    // client -> server: write a line on the client end, read it inbound.
    client_stream
        .write_all(b"hello-in\n")
        .await
        .expect("client write");
    client_stream.flush().await.expect("client flush");
    let inbound = io.rx.next().await.expect("inbound frame");
    assert_eq!(inbound, "hello-in");

    // server -> client: send on the outbound channel, read it on the
    // client end.
    io.tx
        .send("hello-out".to_string())
        .await
        .expect("outbound send");
    let mut buf = [0u8; 32];
    let n = client_stream.read(&mut buf).await.expect("client read");
    assert_eq!(&buf[..n], b"hello-out\n");
}

/// listen_uds accepts a connection and hands it to serve_uds_stream. With
/// no runner registered for the session, serve_session returns early, so
/// the server closes the stream and the client reads EOF. This proves the
/// accept loop, the per-connection spawn, and the serve entry path,
/// without a full runner harness.
#[tokio::test]
async fn test_listen_accepts_one() {
    let dir = socket_dir("hu-uds");
    let path = socket_path(&dir, "l.sock");
    remove_file(&path).ok();

    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    let session = SessionId::new();
    let listen_path = path.clone();
    let listen = tokio::spawn(async move {
        listen_uds(host, session, &listen_path).await.ok();
    });
    // Let the listener bind before the client connects.
    sleep(Duration::from_millis(50)).await;

    let mut client = UnixStream::connect(&path).await.expect("connect");
    // No runner for the session: serve_session returns early, the bridge
    // closes, the client reads EOF.
    let mut buf = [0u8; 16];
    let n = client.read(&mut buf).await.expect("read");
    assert_eq!(n, 0, "server closed the stream with no runner");
    listen.abort();
    remove_file(&path).ok();
}

/// Binding under a parent directory that does not exist fails fast: the
/// error is Unavailable, marked retriable, and names the bind step.
#[tokio::test]
async fn test_bind_failure_reports_unavailable() {
    let missing = temp_dir().join(format!("hu-miss-{}", id()));
    drop(remove_dir_all(&missing));
    let path = missing.join("n.sock");

    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    let err = listen_uds(host, SessionId::new(), &path)
        .await
        .expect_err("bind under a missing parent must fail");
    assert_eq!(err.category, ErrorCategory::Unavailable);
    assert!(err.retriable, "a bind failure is worth retrying");
    assert!(
        err.message.contains("uds bind"),
        "the message names the failing step: {}",
        err.message
    );
}
