//! The raw frame I/O for the in-memory carrier (mode A). One half of the
//! channel pair the composition root mints; the other half goes to the
//! frontend wrapped as the in-memory carrier, so both ends share one pair.
//! Each frame is a complete NDJSON line (newline included), matching the
//! byte stream a pipe would carry.

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::framing::check_frame_size;

/// The service end of the in-memory carrier pair (mode A).
pub struct FrameCarrier {
    /// Outbound frames to the client (service -> client direction).
    pub(crate) tx: mpsc::Sender<String>,
    /// Inbound frames from the client (client -> service direction).
    pub(crate) rx: mpsc::Receiver<String>,
}

impl FrameCarrier {
    /// Build the server end from the channel halves the composition root
    /// allocates. The pair is created once and both ends handed out; the
    /// server never mints its own pair.
    pub fn new(tx: mpsc::Sender<String>, rx: mpsc::Receiver<String>) -> Self {
        Self { tx, rx }
    }

    /// Read the next inbound frame, stripping the trailing newline the carrier
    /// preserves. Ok(None) means the client cleanly half-closed its send side;
    /// an Err means the carrier failed, or the client sent a frame above the
    /// protocol's per-message byte bound. A frame over the bound is a contract
    /// violation rather than a clean close, so it must not read as one.
    pub(crate) async fn next_frame(&mut self) -> Result<Option<String>, ProtocolError> {
        let Some(frame) = self.rx.next().await else {
            return Ok(None);
        };
        let line = frame.strip_suffix('\n').unwrap_or(&frame);
        if let Err(e) = check_frame_size(line) {
            // Ending the connection is the only answer here, so the
            // diagnostic log is what records the size the peer sent.
            tracing::warn!("client frame refused: {e}");
            return Err(e.into());
        }
        Ok(Some(line.to_string()))
    }

    /// Send one outbound frame, appending the newline terminator the carrier
    /// convention requires. A broken sender means the client is gone.
    pub(crate) async fn send_frame(&mut self, frame: String) -> Result<(), ProtocolError> {
        let line = if frame.ends_with('\n') {
            frame
        } else {
            let mut s = frame;
            s.push('\n');
            s
        };
        self.tx
            .send(line)
            .await
            .map_err(|_| ProtocolError::new(ErrorCategory::Unavailable, "client closed", false))
    }
}

#[cfg(test)]
mod tests;
