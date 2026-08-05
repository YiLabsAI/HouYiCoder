//! The raw frame I/O for the in-memory carrier (mode A). One half of the
//! channel pair the composition root mints; the other half goes to the
//! frontend wrapped as the in-memory carrier, so both ends share one pair.
//! Each frame is a complete NDJSON line (newline included), matching the
//! byte-stream wire a pipe would carry.

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_protocol::wire::{WireError, WireErrorKind};

/// The raw frame I/O for the in-memory carrier (mode A). One half of the
/// channel pair the composition root mints; the other half goes to the
/// frontend wrapped as the in-memory carrier, so both ends share one pair.
/// Each frame is a complete NDJSON line (newline included), matching the
/// byte-stream wire a pipe would carry.
pub struct ServerIo {
    /// Outbound frames to the client (service -> client direction).
    pub(crate) tx: mpsc::Sender<String>,
    /// Inbound frames from the client (client -> service direction).
    pub(crate) rx: mpsc::Receiver<String>,
}

impl ServerIo {
    /// Build the server end from the channel halves the composition root
    /// allocates. The pair is created once and both ends handed out; the
    /// server never mints its own pair.
    pub fn new(tx: mpsc::Sender<String>, rx: mpsc::Receiver<String>) -> Self {
        Self { tx, rx }
    }

    /// Read the next inbound frame, stripping the trailing newline the carrier
    /// preserves. None means the client cleanly half-closed its send side.
    pub(crate) async fn next_frame(&mut self) -> Option<String> {
        let frame = self.rx.next().await?;
        Some(frame.strip_suffix('\n').unwrap_or(&frame).to_string())
    }

    /// Send one outbound frame, appending the newline terminator the carrier
    /// convention requires. A broken sender means the client is gone.
    pub(crate) async fn send_frame(&mut self, frame: String) -> Result<(), WireError> {
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
            .map_err(|_| WireError::new(WireErrorKind::Unavailable, "client closed", false))
    }
}
