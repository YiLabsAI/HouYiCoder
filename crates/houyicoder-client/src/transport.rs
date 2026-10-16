//! Transports — the carriers the wire frames flow over. One trait, three
//! implementations: an in-memory channel for mode A; pipes for mode B and a
//! domain socket for detached sessions are not wired yet. A transport
//! moves newline-terminated NDJSON frames as owned strings; encode and decode
//! sit one layer above in protocol, so the transport is carrier mechanics only
//! and stays ignorant of message shape.
//! The contract test runs the same protocol behavior through each transport
//! to assert wire equivalence: a frame the in-memory carrier moves is the
//! same bytes a pipe would move, so a peer cannot tell which carrier
//! delivered it.

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_async::PFut;
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::framing::FrameError;
#[cfg(test)]
use std::io::Cursor;
#[cfg(test)]
use std::io::empty;
#[cfg(test)]
use std::io::sink;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::path::Path;
#[cfg(test)]
use std::pin::pin;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::task::Poll;
use std::thread;

/// A duplex frame transport: send and receive newline-terminated NDJSON
/// frames as owned strings. Each implementation backs the same wire contract
/// — an ordered, lossless, newline-delimited byte stream — behind a
/// different carrier. The caller encodes a message to a frame string (in
/// protocol) and sends it; recv returns the next frame content with the
/// trailing newline stripped, for the caller to decode. A cleanly closed
/// peer reads as None; a broken transport surfaces as an Unavailable wire
/// error.
pub trait Transport: Send {
    /// Send one complete NDJSON frame (newline-terminated). The frame passes
    /// through verbatim; the caller owns encoding. An Err means the carrier
    /// broke during the write or flush; because a partial or even complete
    /// frame may have reached the peer, the per-frame delivery outcome is
    /// Unknown, not provably NotSent. The caller must not auto-retry a
    /// mutation on this error.
    fn send_frame(&mut self, frame: &str) -> PFut<'_, Result<(), ProtocolError>>;

    /// Receive the next complete frame as an owned string with the trailing
    /// newline stripped. None means the peer cleanly half-closed its send
    /// side; an Err means the carrier failed mid-stream, or the peer sent a
    /// frame above the protocol's per-message byte bound. That second case is a
    /// peer contract violation rather than a clean close, so it must not
    /// read as one.
    fn recv_frame(&mut self) -> PFut<'_, Result<Option<String>, ProtocolError>>;
}

/// An in-memory duplex transport backed by a pair of futures channels. This
/// is the mode-A carrier and the contract-test vehicle: both ends of a pair
/// share one process, frames pass as owned strings through the channels.
/// The contract test forces a serialize-to-string-to-deserialize round-trip
/// through this transport, so wire equivalence is asserted rather than
/// assumed — the same bytes that would cross a pipe cross the channel.
pub struct InProcTransport {
    tx: mpsc::Sender<String>,
    rx: mpsc::Receiver<String>,
}

impl InProcTransport {
    /// Create a paired duplex: frames sent by one end arrive at the other in
    /// order. buffer bounds the per-direction channel capacity; a sender
    /// yields to backpressure once full rather than unbounded buffering.
    pub fn pair(buffer: usize) -> (Self, Self) {
        let (tx_a, rx_a) = mpsc::channel(buffer);
        let (tx_b, rx_b) = mpsc::channel(buffer);
        (Self { tx: tx_a, rx: rx_b }, Self { tx: tx_b, rx: rx_a })
    }

    /// Build a transport end from channel halves the caller allocated, so a
    /// composition root can hand the matching halves to the service and both
    /// ends share one channel pair. The halves are one direction each: tx
    /// carries outbound frames, rx yields inbound frames.
    pub fn from_halves(tx: mpsc::Sender<String>, rx: mpsc::Receiver<String>) -> Self {
        Self { tx, rx }
    }
}

impl Transport for InProcTransport {
    fn send_frame(&mut self, frame: &str) -> PFut<'_, Result<(), ProtocolError>> {
        // Lift the borrowed frame to an owned string before the async block so
        // the returned future borrows only self, not the caller's str.
        let owned = frame.to_string();
        Box::pin(async move {
            self.tx.send(owned).await.map_err(|_| {
                ProtocolError::new(
                    ErrorCategory::Unavailable,
                    "in-proc transport peer closed (send failed)",
                    true,
                )
            })
        })
    }

    fn recv_frame(&mut self) -> PFut<'_, Result<Option<String>, ProtocolError>> {
        Box::pin(async move {
            match self.rx.next().await {
                // Strip the trailing newline that encode appended so the
                // caller receives the frame content, not the terminator.
                Some(line) => Ok(Some(line.strip_suffix('\n').unwrap_or(&line).to_string())),
                None => Ok(None),
            }
        })
    }
}

/// A duplex transport over a process's stdin/stdout (the mode-B carrier). A
/// dedicated thread blocks on stdin reads — the async stdin readers have a
/// known event-loop blocking hazard on some platforms, so a blocking reader on
/// its own thread is the robust shape. Complete frames surface on the async
/// recv_frame via an unbounded futures channel; sends block-write stdout
/// (frame-level, short). The carrier honors the same newline-delimited NDJSON
/// contract as the in-memory transport, so a peer cannot tell which carrier
/// delivered a frame.
///
/// The read thread is detached: it exits when stdin hits EOF or errors, and
/// drops the channel sender so recv_frame reads a clean close. The line
/// limit guards against an unbounded peer streaming one frame to exhaust
/// memory (the same guard the framing decoder enforces on the decode side).
pub struct StdioTransport {
    rx: mpsc::UnboundedReceiver<Result<Option<String>, ProtocolError>>,
    stdout: Box<dyn Write + Send>,
}

impl StdioTransport {
    /// Build a mode-B transport over a readable stdin + writable stdout. The
    /// composition root passes the real process stdin/stdout; tests pass
    /// in-memory handles. max_line is the per-frame byte cap before the
    /// terminator.
    pub fn new(
        stdin: Box<dyn Read + Send>,
        stdout: Box<dyn Write + Send>,
        max_line: usize,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdin);
            let mut buf = String::new();
            loop {
                buf.clear();
                match reader.read_line(&mut buf) {
                    Ok(0) => {
                        let _send = tx.unbounded_send(Ok(None));
                        return;
                    }
                    Ok(_) => {
                        let line = buf.strip_suffix('\n').unwrap_or(&buf).to_string();
                        if line.len() > max_line {
                            // The peer sent a frame above the protocol's
                            // per-message bound: not a carrier failure, so the
                            // size refusal keeps its own category and text
                            // rather than reading as a broken stream.
                            let refusal = FrameError::TooLarge {
                                limit: max_line,
                                actual: line.len(),
                            };
                            let _send = tx.unbounded_send(Err(refusal.into()));
                            return;
                        }
                        if tx.unbounded_send(Ok(Some(line))).is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _send = tx.unbounded_send(Err(ProtocolError::new(
                            ErrorCategory::Unavailable,
                            "stdio read failed",
                            true,
                        )));
                        return;
                    }
                }
            }
        });
        Self { rx, stdout }
    }
}

impl Transport for StdioTransport {
    fn send_frame(&mut self, frame: &str) -> PFut<'_, Result<(), ProtocolError>> {
        let owned = frame.to_string();
        Box::pin(async move {
            self.stdout.write_all(owned.as_bytes()).map_err(|_| {
                ProtocolError::new(ErrorCategory::Unavailable, "stdio write failed", true)
            })?;
            self.stdout.flush().map_err(|_| {
                ProtocolError::new(ErrorCategory::Unavailable, "stdio flush failed", true)
            })?;
            Ok(())
        })
    }

    fn recv_frame(&mut self) -> PFut<'_, Result<Option<String>, ProtocolError>> {
        Box::pin(async move {
            match self.rx.next().await {
                // The read thread dropped its sender (EOF or fatal) — a clean
                // close, not an error, matching the pipe-peer semantics.
                None => Ok(None),
                Some(Ok(opt)) => Ok(opt),
                Some(Err(e)) => Err(e),
            }
        })
    }
}

/// A domain-socket transport for a detached session. The client connects to
/// a UnixListener a background agent process bound; frames flow over the
/// socket as NDJSON lines. The carrier mechanics (a thread blocking on
/// line reads + direct writes) are identical to the stdio carrier, so this
/// delegates to a StdioTransport over the socket's read + write halves.
/// Unix-only.
#[cfg(unix)]
pub struct UdsTransport {
    inner: StdioTransport,
}

#[cfg(unix)]
impl UdsTransport {
    /// Connect to a detached session's listening socket. The stream is split
    /// into a cloned read half (the reader thread owns it) and the write half
    /// (sends go through it). max_line is the per-frame byte cap.
    pub fn connect(path: impl AsRef<Path>, max_line: usize) -> std::io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Ok(Self::from_stream(stream, max_line))
    }

    /// Wrap an already-connected UnixStream. Factored out so a test can drive
    /// the carrier over a stream pair without binding a listener.
    pub fn from_stream(stream: UnixStream, max_line: usize) -> Self {
        let read = stream.try_clone().expect("clone uds read half");
        let inner = StdioTransport::new(Box::new(read), Box::new(stream), max_line);
        Self { inner }
    }
}

#[cfg(unix)]
impl Transport for UdsTransport {
    fn send_frame(&mut self, frame: &str) -> PFut<'_, Result<(), ProtocolError>> {
        self.inner.send_frame(frame)
    }
    fn recv_frame(&mut self) -> PFut<'_, Result<Option<String>, ProtocolError>> {
        self.inner.recv_frame()
    }
}

#[cfg(test)]
mod tests;
