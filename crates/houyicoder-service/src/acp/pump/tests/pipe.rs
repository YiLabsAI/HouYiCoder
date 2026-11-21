//! Pump behavior over a real byte pipe, where the reader and the writer
//! each own a thread.

use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::thread::spawn;
use std::time::Duration;
use tokio::task::spawn_blocking;
use tokio::time::timeout;

use super::super::*;

fn encoded<T: serde::Serialize>(msg: &T) -> String {
    serde_json::to_string(msg).expect("encode")
}

/// Frames written to the pipe arrive classified in the order the peer sent
/// them, and the peer closing its end ends the frame stream.
#[tokio::test]
async fn test_pump_reads_pipe_frames() {
    let (mut client_write, server_read) = UnixStream::pair().expect("pair");
    let (server_write, _client_read) = UnixStream::pair().expect("pair");
    let mut pump = spawn_pump(BufReader::new(server_read), server_write);

    let req = encoded(&AcpRequest::new(1, "initialize", serde_json::json!({})));
    let notif = encoded(&AcpNotification::new(
        "session/cancel",
        serde_json::json!({"sessionId": "s"}),
    ));
    let resp = encoded(&AcpResponse::ok(
        AcpRequestId::Number(5),
        serde_json::json!({"ok": true}),
    ));
    client_write
        .write_all(format!("{req}\n{notif}\n{resp}\n").as_bytes())
        .expect("write");
    drop(client_write);

    assert!(matches!(
        pump.next_frame().await,
        Some(InboundFrame::Request(_))
    ));
    assert!(matches!(
        pump.next_frame().await,
        Some(InboundFrame::Notification(_))
    ));
    assert!(matches!(
        pump.next_frame().await,
        Some(InboundFrame::Response(_))
    ));
    assert!(pump.next_frame().await.is_none(), "EOF ends the read side");
}

/// A peer that writes one last frame without a line break and closes still
/// gets that frame delivered, as the line reader it replaced did.
#[tokio::test]
async fn test_pump_delivers_unterminated_frame() {
    let (mut client_write, server_read) = UnixStream::pair().expect("pair");
    let (server_write, _client_read) = UnixStream::pair().expect("pair");
    let mut pump = spawn_pump(BufReader::new(server_read), server_write);

    let req = encoded(&AcpRequest::new(3, "initialize", serde_json::json!({})));
    client_write
        .write_all(req.as_bytes())
        .expect("write no newline");
    drop(client_write);

    match pump.next_frame().await {
        Some(InboundFrame::Request(r)) => assert_eq!(r.id, AcpRequestId::Number(3)),
        other => panic!("expected the unterminated request, got {other:?}"),
    }
    assert!(pump.next_frame().await.is_none());
}

/// An outbound frame is on the wire before the next frame is queued, and it
/// carries exactly one line terminator: a second one would leave a blank line
/// between frames and break a strict line-framed peer.
#[tokio::test]
async fn test_pump_flushes_each_frame() {
    let (_client_write, server_read) = UnixStream::pair().expect("pair");
    let (server_write, mut client_read) = UnixStream::pair().expect("pair");
    let mut pump = spawn_pump(BufReader::new(server_read), server_write);

    pump.send_frame(r#"{"jsonrpc":"2.0","id":1,"method":"x"}"#.into())
        .await
        .expect("send");

    let reading = spawn_blocking(move || {
        let mut buf = vec![0u8; 128];
        let n = client_read.read(&mut buf).expect("read");
        String::from_utf8_lossy(&buf[..n]).into_owned()
    });
    let line = timeout(Duration::from_secs(2), reading)
        .await
        .expect("the frame reached the peer without a closing flush")
        .expect("reader joined");
    assert!(
        line.contains(r#""method":"x""#),
        "the frame arrived: {line:?}"
    );
    assert_eq!(
        line.matches('\n').count(),
        1,
        "one frame carries one terminator: {line:?}"
    );
}

/// A reader that outruns the serve loop blocks on a full channel instead of
/// dropping frames: every line the peer wrote is delivered, in order.
#[tokio::test]
async fn test_pump_applies_backpressure() {
    let (client_write, server_read) = UnixStream::pair().expect("pair");
    let (server_write, _client_read) = UnixStream::pair().expect("pair");
    let mut pump = spawn_pump(BufReader::new(server_read), server_write);

    // More frames than the inbound channel holds, written from a thread so
    // the writer parks in the kernel buffer while the pump drains below.
    let writing = spawn(move || {
        let mut writer = client_write;
        for id in 0..300 {
            let frame = encoded(&AcpRequest::new(id, "initialize", serde_json::json!({})));
            if writer.write_all(format!("{frame}\n").as_bytes()).is_err() {
                return;
            }
        }
    });

    let mut seen = 0;
    while let Some(frame) = pump.next_frame().await {
        match frame {
            InboundFrame::Request(r) => assert_eq!(r.id, AcpRequestId::Number(seen)),
            other => panic!("unexpected frame at {seen}: {other:?}"),
        }
        seen += 1;
    }
    assert_eq!(seen, 300, "every frame survived a full channel");
    writing.join().expect("writer thread");
}
