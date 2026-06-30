//! NDJSON framing — one JSON object per line. The wire frame boundary is a
//! newline; a frame is a single serialized message followed by a line break.
//! Debuggable with plain tools (cat, head) and self-delimiting without a
//! length prefix. The decoder is a byte-buffer that yields complete frames as
//! they arrive, holding a partial frame across pushes until its terminator.
//!
//! Framing parse failures are internal to the transport; the service maps
//! them to a WireError (InvalidFrame) at the boundary so internal error types
//! never cross the wire directly.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::VecDeque;

/// A framing failure: a line that is not valid JSON for the expected type, or
/// a frame that exceeded the line limit. The transport maps this to a
/// WireError::InvalidFrame at the boundary.
#[derive(Debug)]
pub enum FrameError {
    /// The frame is not valid JSON, or does not deserialize to the expected type.
    Json(serde_json::Error),
    /// A single frame exceeded the configured max line length (a denial-of-
    /// service guard: an unbounded peer could stream a frame that exhausts
    /// memory before its terminator).
    TooLarge { limit: usize },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(e) => write!(f, "frame json error: {e}"),
            Self::TooLarge { limit } => {
                write!(f, "frame exceeded line limit {limit}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

impl From<serde_json::Error> for FrameError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

/// Encode a message as one NDJSON frame: serialized JSON followed by a
/// newline. The newline is the frame terminator; receivers split on it.
pub fn encode<T: Serialize>(msg: &T) -> Result<String, FrameError> {
    let mut line = serde_json::to_string(msg)?;
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
                    });
                }
                self.ready.push_back(line);
                self.buf.clear();
            } else if self.buf.len() > self.max_line {
                self.buf.clear();
                return Err(FrameError::TooLarge {
                    limit: self.max_line,
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
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Msg {
        id: u32,
        text: String,
    }

    #[test]
    fn test_encode_ends_with_newline() {
        let m = Msg {
            id: 1,
            text: "hi".into(),
        };
        let f = encode(&m).unwrap();
        assert!(f.ends_with('\n'));
        assert_eq!(f, "{\"id\":1,\"text\":\"hi\"}\n");
    }

    #[test]
    fn test_decoder_yields_frames_order() {
        let mut d = FrameDecoder::new(1024);
        let a = encode(&Msg {
            id: 1,
            text: "a".into(),
        })
        .unwrap();
        let b = encode(&Msg {
            id: 2,
            text: "b".into(),
        })
        .unwrap();
        // Push both frames split across two chunks (mid-frame boundary).
        let combined = format!("{a}{b}");
        d.push(combined.as_bytes()).unwrap();
        let first = d.next_frame::<Msg>().unwrap().unwrap();
        let second = d.next_frame::<Msg>().unwrap().unwrap();
        assert_eq!(first.id, 1);
        assert_eq!(second.id, 2);
        assert!(d.next_frame::<Msg>().is_none(), "no third frame");
    }

    #[test]
    fn test_decoder_holds_until_terminator() {
        let mut d = FrameDecoder::new(1024);
        d.push(b"{\"id\":1,\"text\":\"x\"}").unwrap(); // no newline yet
        assert!(d.next_frame::<Msg>().is_none(), "partial frame not yielded");
        d.push(b"\n").unwrap();
        let m = d.next_frame::<Msg>().unwrap().unwrap();
        assert_eq!(m.id, 1);
    }

    #[test]
    fn test_decoder_rejects_oversized_frame() {
        let mut d = FrameDecoder::new(8);
        // A frame longer than the limit without a terminator trips the guard.
        let err = d.push(b"0123456789abcdef").unwrap_err();
        assert!(matches!(err, FrameError::TooLarge { limit: 8 }));
    }

    #[test]
    fn test_decoder_reports_bad_json() {
        let mut d = FrameDecoder::new(1024);
        d.push(b"not json\n").unwrap();
        let err = d.next_frame::<Msg>().unwrap().unwrap_err();
        assert!(matches!(err, FrameError::Json(_)));
    }
}
