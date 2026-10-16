//! The transports: an in-memory carrier and the stdio carrier move the same
//! frames.

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
    // ProtocolError does not derive PartialEq (it carries a message string);
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
    let mut second = pin!(a.send_frame("second\n"));
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
struct RecorderWrite(Arc<Mutex<Vec<u8>>>);
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
    let buf = Arc::new(Mutex::new(Vec::new()));
    let stdin: Box<dyn Read + Send> = Box::new(empty());
    let stdout: Box<dyn Write + Send> = Box::new(RecorderWrite(buf.clone()));
    let mut t = StdioTransport::new(stdin, stdout, 1024);
    t.send_frame("{\"id\":1}\n").await.expect("send ok");
    let written = buf.lock().unwrap().clone();
    assert_eq!(written, b"{\"id\":1}\n");
}

#[tokio::test]
async fn test_stdio_recv_reads_line() {
    let stdin: Box<dyn Read + Send> = Box::new(Cursor::new(b"hello\n".to_vec()));
    let stdout: Box<dyn Write + Send> = Box::new(sink());
    let mut t = StdioTransport::new(stdin, stdout, 1024);
    let frame = t.recv_frame().await.expect("recv ok").expect("a frame");
    assert_eq!(frame, "hello");
}

#[tokio::test]
async fn test_stdio_eof_reads_none() {
    let stdin: Box<dyn Read + Send> = Box::new(empty());
    let stdout: Box<dyn Write + Send> = Box::new(sink());
    let mut t = StdioTransport::new(stdin, stdout, 1024);
    let frame = t.recv_frame().await.expect("recv ok");
    assert_eq!(frame, None, "EOF must read as a clean close, not an error");
}

#[tokio::test]
async fn test_stdio_oversize_frame_errors() {
    let big = b"x".repeat(100);
    let stdin: Box<dyn Read + Send> = Box::new(Cursor::new([big.as_slice(), b"\n"].concat()));
    let stdout: Box<dyn Write + Send> = Box::new(sink());
    let mut t = StdioTransport::new(stdin, stdout, 10);
    let err = t
        .recv_frame()
        .await
        .expect_err("an oversize frame must read as a protocol error");
    assert_eq!(
        err.category,
        ErrorCategory::MessageTooLarge,
        "a peer contract violation keeps its own category rather than reading as a broken stream"
    );
}

/// A frame round-trips over a Unix domain socket pair: a send on the
/// transport lands as bytes on the peer end, and bytes written on the
/// peer end arrive as a frame (newline stripped) on the transport. This
/// proves the UDS carrier reuses the stdio read-thread + write path.
#[cfg(unix)]
#[tokio::test]
async fn test_uds_round_trips_frame() {
    let (mut peer, client) = UnixStream::pair().expect("pair");
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
