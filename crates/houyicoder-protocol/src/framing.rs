//! NDJSON framing — one JSON object per line. The wire frame boundary is a
//! newline; a frame is a single serialized message followed by a line break.
//! Debuggable with plain tools (cat, head) and self-delimiting without a
//! length prefix. The decoder is a byte-buffer that yields complete frames as
//! they arrive, holding a partial frame across pushes until its terminator.
//! One message is bounded: a frame above MAX_MESSAGE_BYTES is refused where it
//! would be produced and where it would be admitted.
//!
//! Framing failures are internal to the transport; the service maps them to a
//! ProtocolError at the boundary so internal error types never cross it
//! directly — a parse failure reads as InvalidFrame, and a frame above the
//! bound keeps its own category so a size refusal is not reported as a
//! malformed frame.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::VecDeque;
use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;

/// A framing failure: a line that is not valid JSON for the expected type, or
/// a frame that exceeded the line limit. Converted at the boundary to a
/// ProtocolError whose category names which of the two it was.
#[derive(Debug)]
pub enum FrameError {
    /// The frame is not valid JSON, or does not deserialize to the expected type.
    Json(serde_json::Error),
    /// A single frame exceeded the protocol's per-message byte bound (a
    /// denial-of-service guard: an unbounded peer could stream a frame that
    /// exhausts memory before its terminator). The actual size rides along so
    /// a report names the number that tripped the bound.
    TooLarge { limit: usize, actual: usize },
}

impl Display for FrameError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(e) => write!(f, "frame json error: {e}"),
            Self::TooLarge { limit, actual } => {
                write!(
                    f,
                    "frame of {actual} bytes exceeds the {limit}-byte message bound"
                )
            }
        }
    }
}

impl Error for FrameError {}

impl From<serde_json::Error> for FrameError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

/// The protocol's per-message byte bound. A frame above it is refused at both
/// entries: encode will not produce one, and a carrier will not admit one.
/// The bound is per message, so a stream of frames stays unbounded in count;
/// what it caps is the memory a single message can force, which is the peak
/// paid before any resident budget or retention rule can apply.
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// Whether a frame is within the per-message byte bound. The bound counts
/// frame content, not its newline terminator.
pub fn check_frame_size(frame: &str) -> Result<(), FrameError> {
    if frame.len() > MAX_MESSAGE_BYTES {
        return Err(FrameError::TooLarge {
            limit: MAX_MESSAGE_BYTES,
            actual: frame.len(),
        });
    }
    Ok(())
}

/// Encode a message as one NDJSON frame: serialized JSON followed by a
/// newline. The newline is the frame terminator; receivers split on it. A
/// message whose serialized form exceeds the per-message byte bound is
/// refused here, before any carrier sees it, so the frame is provably not
/// sent and a caller reports a local refusal rather than an unknown delivery
/// outcome.
pub fn encode<T: Serialize>(msg: &T) -> Result<String, FrameError> {
    let serialized = serde_json::to_string(msg)?;
    check_frame_size(&serialized)?;
    let mut line = serialized;
    line.push('\n');
    Ok(line)
}

/// A buffered NDJSON decoder. Bytes are pushed in arbitrary chunks; complete
/// frames (terminated by a newline) are drained in arrival order. A partial
/// frame stays buffered until its terminator arrives or the line limit trips.
pub struct FrameDecoder {
    buf: Vec<u8>,
    /// Pending complete lines not yet drained.
    ready: VecDeque<String>,
    max_line: usize,
}

impl FrameDecoder {
    /// A decoder with the given per-line byte limit. The limit guards against
    /// an unbounded peer streaming a single frame to exhaust memory.
    pub fn new(max_line: usize) -> Self {
        Self {
            buf: Vec::new(),
            ready: VecDeque::new(),
            max_line,
        }
    }

    /// Feed bytes to the decoder. Splits complete frames into the ready queue.
    /// Returns an error if a frame exceeds the line limit before its
    /// terminator.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), FrameError> {
        for &b in bytes {
            self.buf.push(b);
            if b == b'\n' {
                let line = String::from_utf8_lossy(&self.buf).into_owned();
                let line = line.strip_suffix('\n').unwrap_or(&line).to_string();
                if line.len() > self.max_line {
                    self.buf.clear();
                    return Err(FrameError::TooLarge {
                        limit: self.max_line,
                        actual: line.len(),
                    });
                }
                self.ready.push_back(line);
                self.buf.clear();
            } else if self.buf.len() > self.max_line {
                let actual = self.buf.len();
                self.buf.clear();
                return Err(FrameError::TooLarge {
                    limit: self.max_line,
                    actual,
                });
            }
        }
        Ok(())
    }

    /// Drain the next complete frame, deserialized to T. None when no
    /// complete frame is ready.
    pub fn next_frame<T: DeserializeOwned>(&mut self) -> Option<Result<T, FrameError>> {
        let line = self.ready.pop_front()?;
        Some(serde_json::from_str(&line).map_err(FrameError::from))
    }
}

#[cfg(test)]
mod tests;
