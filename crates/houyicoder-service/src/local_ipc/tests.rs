//! Tests for this module.

use super::*;
use crate::lifecycle::SessionLeaseStore;
use houyicoder_platform::ipc;
use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all};
use std::io::Read;
use std::process::id;
use std::time::Duration;
use tokio::task::spawn_blocking;
use tokio::time::timeout;

#[cfg(unix)]
use std::fs::metadata;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::net::UnixStream as StdUnixStream;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixStream;

/// A frame round-trips through the carrier: writing on one stream end
/// lands on the FrameCarrier inbound channel, and sending on the outbound
/// channel lands on the other stream end. Proves the byte path without a
/// host or runner.
#[cfg(unix)]
#[tokio::test]
async fn test_bridge_round_trips_frame() {
    let (a, b) = StdUnixStream::pair().expect("pair");
    a.set_nonblocking(true).ok();
    b.set_nonblocking(true).ok();
    let server_stream = UnixStream::from_std(a).expect("from_std a");
    let mut client_stream = UnixStream::from_std(b).expect("from_std b");

    let mut io = super::bridge(server_stream).await;

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

/// The listener accepts a connection and hands it to the serve path, and the
/// client reads end of stream once the serve side finishes. With no runner
/// registered for the session, serve_session returns early, so the client
/// learning that the carrier closed is the guarantee under test: a client end
/// the server never closes blocks its reader forever.
#[tokio::test]
async fn test_listener_signals_close() {
    let dir = temp_dir().join(format!("hu-ipc-close-{}", id()));
    create_dir_all(&dir).ok();
    let listener = ipc::bind_session(&dir, "close").expect("bind the endpoint");
    let endpoint = listener.endpoint().to_string();

    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    let session = SessionId::new();
    let serving = tokio::spawn(async move {
        listen_local(host, session, listener).await.ok();
    });
    let connect_to = endpoint.clone();
    let streams = spawn_blocking(move || ipc::connect_local_streams(&connect_to))
        .await
        .expect("the connect task")
        .expect("connect the client");
    let mut reader = streams.read;

    let read = timeout(
        Duration::from_secs(5),
        spawn_blocking(move || {
            let mut buf = [0u8; 16];
            reader.read(&mut buf)
        }),
    )
    .await
    .expect("the serve side must close the stream")
    .expect("the read task");
    assert_eq!(
        read.expect("read the stream"),
        0,
        "the client reads end of stream"
    );
    serving.abort();
    ipc::remove_endpoint(&endpoint).ok();
    remove_dir_all(&dir).ok();
}

/// The endpoint a listener records lives in a directory restricted to its
/// owner, so the record and the socket are never readable by another local
/// account.
#[tokio::test]
async fn test_private_dir_holds_endpoint() {
    let dir = temp_dir().join(format!("hu-ipc-private-{}", id()));
    ipc::create_owner_only_dir(&dir).expect("create the private dir");
    let listener = ipc::bind_session(&dir, "private").expect("bind the endpoint");
    let endpoint = listener.endpoint().to_string();
    assert!(!endpoint.is_empty());
    #[cfg(unix)]
    {
        let mode = metadata(&dir).expect("read the dir").mode() & 0o777;
        assert_eq!(mode, 0o700, "the record dir stays owner-only: {mode:o}");
    }
    drop(listener);
    ipc::remove_endpoint(&endpoint).ok();
    remove_dir_all(&dir).ok();
}
