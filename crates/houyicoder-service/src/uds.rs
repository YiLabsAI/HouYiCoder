//! Unix-domain-socket carrier for detached sessions. A background agent
//! process binds a UnixListener at a socket path; each accepted connection
//! runs serve_session over the stream, so the in-process reconnect-replay
//! path (serve_session + the parked PendingTurn) extends to cross-connection
//! clients without a new server shape. The stream is framed as NDJSON lines,
//! the same convention the in-memory + stdio carriers use.
//!
//! The bridge is factored out of serve so the carrier (stream to channel
//! pair) is testable in isolation, without a host or runner: the serve
//! behavior itself is covered by the in-process reconnect tests, and this
//! carrier only carries bytes.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_context::SessionId;
use houyicoder_protocol::wire::{WireError, WireErrorKind};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::net::UnixStream;

use crate::composition::SessionHost;
use crate::server::ServerIo;
use crate::server::session::serve_session;

/// Bridge a connected UnixStream to a ServerIo channel pair and run
/// serve_session over it. Two tasks carry bytes: a reader pulling NDJSON
/// lines into the inbound channel (client to server), and a writer draining
/// the outbound channel onto the stream (server to client). When serve_session
/// returns, both channels close and the bridge tasks exit (the reader next
/// send errors, the writer next drain yields None).
pub async fn serve_uds_stream(
    host: Arc<SessionHost>,
    session: SessionId,
    stream: UnixStream,
) -> Result<(), WireError> {
    let io = bridge_uds(stream).await;
    serve_session(host, session, io).await
}

/// Bind a UnixListener at the given path and serve every accepted connection
/// on the same session and host. A background agent process calls this to
/// run detached; each connecting client reattaches via serve_session (the
/// lease guard serializes concurrent connections to one session). Blocks
/// until the listener fails.
pub async fn listen_uds(
    host: Arc<SessionHost>,
    session: SessionId,
    path: impl AsRef<Path>,
) -> Result<(), WireError> {
    let listener = UnixListener::bind(path.as_ref())
        .map_err(|e| WireError::new(WireErrorKind::Unavailable, format!("uds bind: {e}"), true))?;
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let host = host.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_uds_stream(host, session, stream).await {
                        eprintln!("uds serve closed: {e}");
                    }
                });
            }
            Err(e) => {
                eprintln!("uds accept failed: {e}; stopping listener");
                return Err(WireError::new(
                    WireErrorKind::Unavailable,
                    format!("uds accept: {e}"),
                    true,
                ));
            }
        }
    }
}

/// Build a ServerIo from a connected UnixStream, spawning the two carrier
/// tasks. Factored out so a test can drive the carrier in isolation without
/// a host or runner.
async fn bridge_uds(stream: UnixStream) -> ServerIo {
    let (read_half, write_half) = stream.into_split();
    // Bounded so a slow client back-pressures the server rather than
    // unbounded-buffering frames the client has not read.
    let (inbound_tx, inbound_rx) = mpsc::channel::<String>(16);
    let (outbound_tx, outbound_rx) = mpsc::channel::<String>(16);

    let mut reader = BufReader::new(read_half);
    tokio::spawn(async move {
        let mut tx = inbound_tx;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let frame = line.strip_suffix('\n').unwrap_or(&line).to_string();
                    if tx.send(frame).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = write_half;
    tokio::spawn(async move {
        let mut rx = outbound_rx;
        while let Some(frame) = rx.next().await {
            let mut line = frame;
            if !line.ends_with('\n') {
                line.push('\n');
            }
            if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
    });

    ServerIo::new(outbound_tx, inbound_rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame round-trips through the carrier: writing on one stream end
    /// lands on the ServerIo inbound channel, and sending on the outbound
    /// channel lands on the other stream end. Proves the byte path without a
    /// host or runner.
    #[tokio::test]
    async fn test_bridge_round_trips_frame() {
        let (a, b) = std::os::unix::net::UnixStream::pair().expect("pair");
        a.set_nonblocking(true).ok();
        b.set_nonblocking(true).ok();
        let server_stream = UnixStream::from_std(a).expect("from_std a");
        let mut client_stream = UnixStream::from_std(b).expect("from_std b");

        let mut io = bridge_uds(server_stream).await;

        // client -> server: write a line on the client end, read it inbound.
        use tokio::io::AsyncWriteExt;
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
        use tokio::io::AsyncReadExt;
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
        use crate::lifecycle::SessionLeaseStore;
        use std::time::Duration;
        use tokio::io::AsyncReadExt;

        let dir = std::env::temp_dir().join(format!("houyicoder-uds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("listen_accepts_one.sock");
        std::fs::remove_file(&path).ok();

        let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
        let session = SessionId::new();
        let listen_path = path.clone();
        let listen = tokio::spawn(async move {
            listen_uds(host, session, &listen_path).await.ok();
        });
        // Let the listener bind before the client connects.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = UnixStream::connect(&path).await.expect("connect");
        // No runner for the session: serve_session returns early, the bridge
        // closes, the client reads EOF.
        let mut buf = [0u8; 16];
        let n = client.read(&mut buf).await.expect("read");
        assert_eq!(n, 0, "server closed the stream with no runner");
        listen.abort();
        std::fs::remove_file(&path).ok();
    }
}
