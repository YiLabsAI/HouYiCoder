//! The raw frame I/O for the in-memory carrier (mode A). One half of the
//! channel pair the composition root mints; the other half goes to the
//! frontend wrapped as the in-memory carrier, so both ends share one pair.
//! Each frame is a complete NDJSON line (newline included), matching the
//! byte stream a pipe would carry.

use std::collections::VecDeque;

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::framing::check_frame_size;

/// Bound on client request frames held aside while a reverse ask is open.
const MAX_DEFERRED_FRAMES: usize = 64;

/// The service end of the in-memory carrier pair (mode A).
pub struct FrameCarrier {
    /// Outbound frames to the client (service -> client direction).
    pub(crate) tx: mpsc::Sender<String>,
    /// Inbound frames from the client (client -> service direction).
    pub(crate) rx: mpsc::Receiver<String>,
    /// Client request frames that crossed on the wire while a reverse ask was
    /// waiting for its response, drained ahead of new inbound frames.
    deferred: VecDeque<String>,
}

impl FrameCarrier {
    /// Build the server end from the channel halves the composition root
    /// allocates. The pair is created once and both ends handed out; the
    /// server never mints its own pair.
    pub fn new(tx: mpsc::Sender<String>, rx: mpsc::Receiver<String>) -> Self {
        Self {
            tx,
            rx,
            deferred: VecDeque::new(),
        }
    }

    /// Read the next frame for normal dispatch, draining any frame deferred
    /// during an earlier reverse ask before reading from the inbound stream.
    pub(crate) async fn next_frame(&mut self) -> Result<Option<String>, ProtocolError> {
        if let Some(frame) = self.deferred.pop_front() {
            return Ok(Some(frame));
        }
        self.next_live_frame().await
    }

    /// Read the next inbound frame directly from the transport, stripping the
    /// trailing newline the carrier preserves. Used by reverse-ask loops so a
    /// frame they just deferred is not handed back to the same wait loop.
    pub(crate) async fn next_live_frame(&mut self) -> Result<Option<String>, ProtocolError> {
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

    /// Hold a client request frame aside until the active reverse ask settles.
    /// Returns false when the queue is full.
    pub(crate) fn defer_frame(&mut self, frame: String) -> bool {
        if self.deferred.len() >= MAX_DEFERRED_FRAMES {
            return false;
        }
        self.deferred.push_back(frame);
        true
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
