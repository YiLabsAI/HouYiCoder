//! The cancel-safe duplex frame pump. A dedicated reader thread feeds a
//! bounded inbound channel through the line decoder, a dedicated writer
//! thread flushes each frame as it is produced, and a pending-rpc table
//! routes responses back to the request that awaits them. The reader never
//! competes with other futures inside a select branch, so canceling an
//! operation mid-read cannot drop bytes the peer already sent.

use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::thread::JoinHandle;
use std::thread::spawn;

use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::acp_wire::AcpRequest;
use houyicoder_protocol::acp_wire::AcpRequestId;
use houyicoder_protocol::acp_wire::AcpResponse;
use houyicoder_protocol::error::ErrorCategory;
use houyicoder_protocol::error::ProtocolError;
use houyicoder_protocol::framing::FrameDecoder;
use houyicoder_protocol::framing::MAX_MESSAGE_BYTES;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::spawn_blocking;

/// The read size the reader hands the decoder. The decoder owns the
/// per-message bound, so the reader must not buffer a whole line itself:
/// a fixed chunk keeps memory flat when a peer streams a line that never
/// terminates.
const READ_CHUNK_BYTES: usize = 8 * 1024;

/// One decoded inbound frame, classified by the envelope it carries.
#[derive(Debug, Clone)]
pub enum InboundFrame {
    /// A request carrying a correlation id.
    Request(AcpRequest),
    /// A response to a reverse request this side sent.
    Response(AcpResponse),
    /// A notification with no reply.
    Notification(AcpNotification),
    /// A line that parsed as none of the three envelope shapes, kept so the
    /// serve loop can answer it with a parse error instead of dropping it.
    Unparseable(String),
}

/// The duplex frame pump over a byte stream.
pub struct AcpDuplexPump {
    inbound_rx: mpsc::Receiver<InboundFrame>,
    outbound_tx: mpsc::Sender<String>,
    pending_rpcs: HashMap<i64, oneshot::Sender<AcpResponse>>,
    writer: Option<JoinHandle<()>>,
}

impl AcpDuplexPump {
    /// Read the next decoded inbound frame. None means the peer closed its
    /// send side and every buffered frame has been consumed.
    pub async fn next_frame(&mut self) -> Option<InboundFrame> {
        self.inbound_rx.recv().await
    }

    /// Send one outbound frame as a complete NDJSON line. The writer owns
    /// the byte stream, so this only queues; the flush happens there the
    /// moment the frame arrives. The frame carries exactly one terminator:
    /// a caller may pass one that already ends with a line break.
    pub async fn send_frame(&mut self, frame: String) -> Result<(), ProtocolError> {
        let line = if frame.ends_with('\n') {
            frame
        } else {
            format!("{frame}\n")
        };
        self.outbound_tx
            .send(line)
            .await
            .map_err(|_| ProtocolError::new(ErrorCategory::Unavailable, "client closed", false))
    }

    /// Register a slot for the response to a reverse request this side sent.
    /// The waiter receives the response when it arrives, or None when the
    /// connection closed first.
    pub fn expect_response(&mut self, id: i64) -> oneshot::Receiver<AcpResponse> {
        let (tx, rx) = oneshot::channel();
        self.pending_rpcs.insert(id, tx);
        rx
    }

    /// Drop the slot for a reverse request that ended without an answer, so
    /// an abandoned ask does not leave a sender behind.
    pub fn forget_response(&mut self, id: i64) {
        self.pending_rpcs.remove(&id);
    }

    /// Close the outbound side and wait for the writer to flush every frame
    /// already queued. Call it after the serve loop returns, from inside the
    /// runtime that served the connection, so a reply the loop queued last is
    /// on the wire before the process exits. The reader is not joined: it
    /// ends at peer EOF, a read error, or the next frame that finds the serve
    /// loop gone, and a peer that never closes could hold it open forever.
    pub async fn close(self) {
        let Self {
            outbound_tx,
            writer,
            ..
        } = self;
        drop(outbound_tx);
        if let Some(writer) = writer {
            match spawn_blocking(move || writer.join()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => tracing::warn!("writer thread panicked"),
                Err(e) => tracing::warn!(error = %e, "writer thread join failed"),
            }
        }
    }

    /// Deliver an inbound response to its waiter. Returns false when no
    /// waiter is registered, which the caller logs rather than acting on.
    pub fn deliver_response(&mut self, resp: AcpResponse) -> bool {
        let id = match &resp {
            AcpResponse::Result { id, .. } | AcpResponse::Error { id, .. } => id,
        };
        if let AcpRequestId::Number(n) = id
            && let Some(tx) = self.pending_rpcs.remove(n)
        {
            return tx.send(resp).is_ok();
        }
        false
    }
}

/// Spawn a pump over a blocking byte reader and a blocking byte writer.
///
/// The reader owns one thread: it reads fixed-size chunks into the frame
/// decoder, which enforces the per-message bound and yields complete lines,
/// and forwards each classified frame to a bounded channel whose send blocks
/// when the serve loop is behind. The writer owns a second thread and
/// flushes each queued frame the moment it arrives. Both threads stop when
/// the peer closes, a read or write fails, or the pump drops and closes the
/// channels.
///
/// Build the pump from inside a Tokio runtime when the result will be used
/// there: dropping the pump outside one is fine, but the writer thread is
/// joined through the runtime on close.
pub fn spawn_pump<R, W>(reader: R, writer: W) -> AcpDuplexPump
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let (inbound_tx, inbound_rx) = mpsc::channel(256);
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<String>(256);

    spawn(move || {
        let mut decoder = FrameDecoder::new(MAX_MESSAGE_BYTES);
        let mut reader = reader;
        let mut chunk = vec![0u8; READ_CHUNK_BYTES];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => {
                    // A final line the peer wrote without a terminator still
                    // counts as a frame; the old line reader delivered it, and
                    // a client that sends one request then closes expects the
                    // reply.
                    if let Some(line) = decoder.finish()
                        && inbound_tx.blocking_send(classify_frame(&line)).is_err()
                    {
                        return;
                    }
                    break;
                }
                Ok(n) => {
                    if decoder.push(&chunk[..n]).is_err() {
                        tracing::warn!("inbound frame exceeded the line limit; connection closed");
                        break;
                    }
                    if !forward_lines(&mut decoder, &inbound_tx) {
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "reader failed; connection closed");
                    break;
                }
            }
        }
    });

    let writer_thread = spawn(move || {
        let mut writer = writer;
        while let Some(frame) = outbound_rx.blocking_recv() {
            let line = if frame.ends_with('\n') {
                frame
            } else {
                format!("{frame}\n")
            };
            if let Err(e) = writer.write_all(line.as_bytes()) {
                tracing::warn!(error = %e, "outbound write failed");
                break;
            }
            if let Err(e) = writer.flush() {
                tracing::warn!(error = %e, "outbound flush failed");
                break;
            }
        }
    });

    AcpDuplexPump {
        inbound_rx,
        outbound_tx,
        pending_rpcs: HashMap::new(),
        writer: Some(writer_thread),
    }
}

/// Build a pump over line channels, for an in-memory carrier where the peer
/// runs in this process. The inbound side is classified by a task that
/// converts each line into a typed frame, so the serve loop reads the same
/// frame types a pipe-backed pump produces. Call it from inside a Tokio
/// runtime: it starts the classifying task.
pub fn from_line_channels(
    inbound: mpsc::Receiver<String>,
    outbound: mpsc::Sender<String>,
) -> AcpDuplexPump {
    let (inbound_tx, inbound_rx) = mpsc::channel(256);
    let mut inbound = inbound;
    tokio::spawn(async move {
        while let Some(line) = inbound.recv().await {
            let line = line.strip_suffix('\n').unwrap_or(&line);
            if inbound_tx.send(classify_frame(line)).await.is_err() {
                return;
            }
        }
    });
    AcpDuplexPump {
        inbound_rx,
        outbound_tx: outbound,
        pending_rpcs: HashMap::new(),
        writer: None,
    }
}

/// Forward every complete line the decoder holds. Returns false when the
/// serve loop dropped its receiver, which ends the reader.
fn forward_lines(decoder: &mut FrameDecoder, tx: &mpsc::Sender<InboundFrame>) -> bool {
    while let Some(line) = decoder.next_line() {
        if tx.blocking_send(classify_frame(&line)).is_err() {
            return false;
        }
    }
    true
}

/// Classify one decoded line as a request, response, notification, or an
/// unparseable line. The response shape is tried first because it is the
/// only one that carries a result or an error field.
fn classify_frame(line: &str) -> InboundFrame {
    if let Ok(resp) = serde_json::from_str::<AcpResponse>(line) {
        return InboundFrame::Response(resp);
    }
    if let Ok(req) = serde_json::from_str::<AcpRequest>(line) {
        return InboundFrame::Request(req);
    }
    if let Ok(notif) = serde_json::from_str::<AcpNotification>(line) {
        return InboundFrame::Notification(notif);
    }
    InboundFrame::Unparseable(line.to_string())
}

#[cfg(test)]
mod tests;
