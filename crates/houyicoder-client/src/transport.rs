//! Transports — the carriers the wire frames flow over. One trait, three
//! implementations across the milestones: an in-memory channel for mode A
//! (here), pipes for mode B (later), and a domain socket for detached
//! sessions (later). A transport moves newline-terminated NDJSON frames as
//! owned strings; encode and decode sit one layer above in protocol, so the
//! transport is carrier mechanics only and stays ignorant of message shape.
//! The contract test runs the same protocol behavior through each transport
//! to assert wire equivalence: a frame the in-memory carrier moves is the
//! same bytes a pipe would move, so a peer cannot tell which carrier
//! delivered it.

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_async::PFut;
use houyicoder_protocol::wire::{WireError, WireErrorKind};
use std::io::{BufRead, BufReader, Read, Write};
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
    /// is broken and the peer will not receive this or any later frame.
    fn send_frame(&mut self, frame: &str) -> PFut<'_, Result<(), WireError>>;

    /// Receive the next complete frame as an owned string with the trailing
    /// newline stripped. None means the peer cleanly half-closed its send
    /// side; an Err means the carrier failed mid-stream.
    fn recv_frame(&mut self) -> PFut<'_, Result<Option<String>, WireError>>;
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
    fn send_frame(&mut self, frame: &str) -> PFut<'_, Result<(), WireError>> {
        // Lift the borrowed frame to an owned string before the async block so
        // the returned future borrows only self, not the caller's str.
        let owned = frame.to_string();
        Box::pin(async move {
            self.tx.send(owned).await.map_err(|_| {
                WireError::new(
                    WireErrorKind::Unavailable,
                    "in-proc transport peer closed (send failed)",
                    true,
                )
            })
        })
    }

    fn recv_frame(&mut self) -> PFut<'_, Result<Option<String>, WireError>> {
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
    rx: mpsc::UnboundedReceiver<Result<Option<String>, WireError>>,
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
                            let _send = tx.unbounded_send(Err(WireError::new(
                                WireErrorKind::InvalidFrame,
                                format!("frame exceeds {max_line}-byte limit"),
                                false,
                            )));
                            return;
                        }
                        if tx.unbounded_send(Ok(Some(line))).is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _send = tx.unbounded_send(Err(WireError::new(
                            WireErrorKind::Unavailable,
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
    fn send_frame(&mut self, frame: &str) -> PFut<'_, Result<(), WireError>> {
        let owned = frame.to_string();
        Box::pin(async move {
            self.stdout.write_all(owned.as_bytes()).map_err(|_| {
                WireError::new(WireErrorKind::Unavailable, "stdio write failed", true)
            })?;
            self.stdout.flush().map_err(|_| {
                WireError::new(WireErrorKind::Unavailable, "stdio flush failed", true)
            })?;
            Ok(())
        })
    }

    fn recv_frame(&mut self) -> PFut<'_, Result<Option<String>, WireError>> {
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
    pub fn connect(path: impl AsRef<std::path::Path>, max_line: usize) -> std::io::Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(path)?;
        Ok(Self::from_stream(stream, max_line))
    }

    /// Wrap an already-connected UnixStream. Factored out so a test can drive
    /// the carrier over a stream pair without binding a listener.
    pub fn from_stream(stream: std::os::unix::net::UnixStream, max_line: usize) -> Self {
        let read = stream.try_clone().expect("clone uds read half");
        let inner = StdioTransport::new(Box::new(read), Box::new(stream), max_line);
        Self { inner }
    }
}

#[cfg(unix)]
impl Transport for UdsTransport {
    fn send_frame(&mut self, frame: &str) -> PFut<'_, Result<(), WireError>> {
        self.inner.send_frame(frame)
    }
    fn recv_frame(&mut self) -> PFut<'_, Result<Option<String>, WireError>> {
        self.inner.recv_frame()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A paired duplex delivers frames in order, each with the terminator
    /// stripped, so the content survives a round-trip through the carrier.
    #[tokio::test]
    async fn test_pair_delivers_in_order() {
        let (mut a, mut b) = InProcTransport::pair(4);
        let frame = "{\"id\":1,\"text\":\"hello\"}\n";
        a.send_frame(frame).await.expect("send ok");
        let recv = b.recv_frame().await.expect("recv ok").expect("a frame");
        assert_eq!(recv, "{\"id\":1,\"text\":\"hello\"}");
    }

    /// Dropping a peer sender surfaces as a clean close (None) on the
    /// receiver, not an error — the same semantics a pipe peer has when the
    /// other end exits.
    #[tokio::test]
    async fn test_peer_close_reads_none() {
        let (a, mut b) = InProcTransport::pair(4);
        drop(a);
        // WireError does not derive PartialEq (it carries a message string);
        // assert the clean-close shape without comparing errors by value.
        let recv = b.recv_frame().await;
        assert!(recv.is_ok(), "clean close is not an error");
        assert_eq!(recv.unwrap(), None);
    }

    /// A bounded channel of one holds one frame; a second send yields until
    /// the receiver drains. This is the carrier honoring flow control rather
    /// than unbounded buffering.
    #[tokio::test]
    async fn test_backpressure_suspends_send() {
        let (mut a, mut b) = InProcTransport::pair(1);
        a.send_frame("first\n").await.expect("send first ok");
        // Buffer is full; the second send cannot complete until b drains.
        use std::task::Poll;
        let mut second = std::pin::pin!(a.send_frame("second\n"));
        match futures::poll!(second.as_mut()) {
            Poll::Ready(r) => panic!("second send should be pending, got {r:?}"),
            Poll::Pending => {}
        }
        let first = b.recv_frame().await.unwrap().unwrap();
        assert_eq!(first, "first");
        // Now drained, the second send completes.
        second.await.expect("second completes after drain");
        let second_recv = b.recv_frame().await.unwrap().unwrap();
        assert_eq!(second_recv, "second");
    }

    /// A write recorder so a send_frame test can assert the bytes that hit
    /// stdout. Shared behind a mutex so the transport owns it by Box and the
    /// test reads it after.
    struct RecorderWrite(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for RecorderWrite {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_stdio_send_writes_stdout() {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stdin: Box<dyn Read + Send> = Box::new(std::io::empty());
        let stdout: Box<dyn Write + Send> = Box::new(RecorderWrite(buf.clone()));
        let mut t = StdioTransport::new(stdin, stdout, 1024);
        t.send_frame("{\"id\":1}\n").await.expect("send ok");
        let written = buf.lock().unwrap().clone();
        assert_eq!(written, b"{\"id\":1}\n");
    }

    #[tokio::test]
    async fn test_stdio_recv_reads_line() {
        let stdin: Box<dyn Read + Send> = Box::new(std::io::Cursor::new(b"hello\n".to_vec()));
        let stdout: Box<dyn Write + Send> = Box::new(std::io::sink());
        let mut t = StdioTransport::new(stdin, stdout, 1024);
        let frame = t.recv_frame().await.expect("recv ok").expect("a frame");
        assert_eq!(frame, "hello");
    }

    #[tokio::test]
    async fn test_stdio_eof_reads_none() {
        let stdin: Box<dyn Read + Send> = Box::new(std::io::empty());
        let stdout: Box<dyn Write + Send> = Box::new(std::io::sink());
        let mut t = StdioTransport::new(stdin, stdout, 1024);
        let frame = t.recv_frame().await.expect("recv ok");
        assert_eq!(frame, None, "EOF must read as a clean close, not an error");
    }

    #[tokio::test]
    async fn test_stdio_oversize_frame_errors() {
        let big = b"x".repeat(100);
        let stdin: Box<dyn Read + Send> =
            Box::new(std::io::Cursor::new([big.as_slice(), b"\n"].concat()));
        let stdout: Box<dyn Write + Send> = Box::new(std::io::sink());
        let mut t = StdioTransport::new(stdin, stdout, 10);
        let res = t.recv_frame().await;
        assert!(
            res.is_err(),
            "an oversize frame must surface as a wire error"
        );
    }

    /// A frame round-trips over a Unix domain socket pair: a send on the
    /// transport lands as bytes on the peer end, and bytes written on the
    /// peer end arrive as a frame (newline stripped) on the transport. This
    /// proves the UDS carrier reuses the stdio read-thread + write path.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_uds_round_trips_frame() {
        use std::io::{Read, Write};
        let (mut peer, client) = std::os::unix::net::UnixStream::pair().expect("pair");
        let mut t = UdsTransport::from_stream(client, 1024);
        // client -> peer: send via transport, read raw on the peer end.
        t.send_frame("{\"hi\":1}\n").await.expect("send");
        let mut buf = [0u8; 32];
        let n = peer.read(&mut buf).expect("peer read");
        assert_eq!(&buf[..n], b"{\"hi\":1}\n");
        // peer -> client: write raw, recv via transport (newline stripped).
        peer.write_all(b"{\"ok\":2}\n").expect("peer write");
        let frame = t.recv_frame().await.expect("recv").expect("a frame");
        assert_eq!(frame, "{\"ok\":2}");
    }
}
