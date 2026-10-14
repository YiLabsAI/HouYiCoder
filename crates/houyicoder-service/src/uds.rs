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
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::net::UnixStream;

use crate::composition::SessionHost;
use crate::server::FrameCarrier;
use crate::server::session::serve_session;

/// Bridge a connected UnixStream to a FrameCarrier channel pair and run
/// serve_session over it. Two tasks carry bytes: a reader pulling NDJSON
/// lines into the inbound channel (client to server), and a writer draining
/// the outbound channel onto the stream (server to client). When serve_session
/// returns, both channels close and the bridge tasks exit (the reader next
/// send errors, the writer next drain yields None).
pub async fn serve_uds_stream(
    host: Arc<SessionHost>,
    session: SessionId,
    stream: UnixStream,
) -> Result<(), ProtocolError> {
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
) -> Result<(), ProtocolError> {
    let listener = UnixListener::bind(path.as_ref()).map_err(|e| {
        ProtocolError::new(ErrorCategory::Unavailable, format!("uds bind: {e}"), true)
    })?;
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
                return Err(ProtocolError::new(
                    ErrorCategory::Unavailable,
                    format!("uds accept: {e}"),
                    true,
                ));
            }
        }
    }
}

/// Build a FrameCarrier from a connected UnixStream, spawning the two carrier
/// tasks. Factored out so a test can drive the carrier in isolation without
/// a host or runner.
async fn bridge_uds(stream: UnixStream) -> FrameCarrier {
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

    FrameCarrier::new(outbound_tx, inbound_rx)
}

#[cfg(test)]
mod tests;
