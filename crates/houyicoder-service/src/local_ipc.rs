//! Local inter-process carrier for detached sessions. A background agent
//! process binds the endpoint its platform provides, a Unix domain socket or a
//! Windows named pipe, and each accepted connection runs serve_session over the
//! stream, so the reconnect-replay path extends to cross-process clients.
//! Frames are NDJSON lines, and the permission or access list an endpoint
//! carries belongs to the platform layer, so this module holds no platform
//! conditional.

use std::io::Error as IoError;
use std::sync::Arc;

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_context::SessionId;
use houyicoder_platform::ipc::LocalListener;
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, split};

use crate::composition::SessionHost;
use crate::server::FrameCarrier;
use crate::server::session::serve_session;

/// Serve one connected client over the carriage channel.
pub async fn serve_local_stream(
    host: Arc<SessionHost>,
    session: SessionId,
    stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
) -> Result<(), ProtocolError> {
    let io = bridge(stream).await;
    serve_session(host, session, io).await
}

/// Serve every client that connects to a bound local endpoint on the same
/// session and host. A background agent process calls this to run detached;
/// each connecting client reattaches via serve_session (the lease guard
/// serializes concurrent connections to one session). Blocks until the
/// endpoint fails.
///
/// The caller binds the endpoint and records what it bound only after the bind
/// succeeds, so a session that cannot listen is never discoverable.
pub async fn listen_local(
    host: Arc<SessionHost>,
    session: SessionId,
    mut listener: LocalListener,
) -> Result<(), ProtocolError> {
    loop {
        let stream = listener.accept().await.map_err(endpoint_failure)?;
        let host = host.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_local_stream(host, session, stream).await {
                tracing::warn!(error = %e, "local session stream closed");
            }
        });
    }
}

/// The error a failed bind or accept reports. A local endpoint failure is
/// worth retrying: the usual cause is a socket file or pipe name still held by
/// a process that is shutting down.
fn endpoint_failure(e: IoError) -> ProtocolError {
    ProtocolError::new(
        ErrorCategory::Unavailable,
        format!("local endpoint: {e}"),
        true,
    )
}

/// Build a FrameCarrier from a connected stream, spawning the two carrier
/// tasks. Factored out so a test can drive the carrier in isolation without a
/// host or runner.
async fn bridge<S>(stream: S) -> FrameCarrier
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read_half, write_half) = split(stream);
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
        // The peer reads end of stream when the outbound channel closes. A
        // split half does not close the stream as it drops, because the read
        // half still holds it, so the write direction is shut down here; each
        // platform's stream turns that into the close its peer observes.
        if let Err(e) = writer.shutdown().await {
            tracing::warn!(error = %e, "local carrier writer shutdown failed");
        }
    });

    FrameCarrier::new(outbound_tx, inbound_rx)
}

#[cfg(test)]
mod tests;
