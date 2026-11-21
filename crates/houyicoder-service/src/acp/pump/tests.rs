//! Tests for this module.

#[cfg(unix)]
mod pipe;

use super::*;

fn encoded<T: serde::Serialize>(msg: &T) -> String {
    serde_json::to_string(msg).expect("encode")
}

/// The pump crosses task and thread boundaries, so its handle is Send and
/// Sync and needs no single-threaded runtime.
#[test]
fn test_pump_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AcpDuplexPump>();
}

fn line_pair() -> (AcpDuplexPump, mpsc::Sender<String>, mpsc::Receiver<String>) {
    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    (
        from_line_channels(server_rx, server_tx),
        client_tx,
        client_rx,
    )
}

/// Each inbound line is classified by the envelope it carries, so the serve
/// loop reads typed frames rather than re-parsing raw lines, and a line that
/// matches no envelope arrives as an unparseable frame instead of vanishing.
#[tokio::test]
async fn test_pump_classifies_frames() {
    let (mut pump, client_tx, _client_rx) = line_pair();

    let req = encoded(&AcpRequest::new(1, "initialize", serde_json::json!({})));
    let notif = encoded(&AcpNotification::new(
        "session/cancel",
        serde_json::json!({"sessionId": "s"}),
    ));
    let resp = encoded(&AcpResponse::ok(
        AcpRequestId::Number(5),
        serde_json::json!({"ok": true}),
    ));
    for line in [req, notif, resp, "not json".to_string()] {
        client_tx
            .send(format!("{line}\n"))
            .await
            .expect("client sends a frame");
    }

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
    assert!(matches!(
        pump.next_frame().await,
        Some(InboundFrame::Unparseable(_))
    ));
    drop(client_tx);
    assert!(
        pump.next_frame().await.is_none(),
        "close ends the read side"
    );
}

/// An outbound frame keeps the NDJSON terminator and reaches the peer as one
/// complete line.
#[tokio::test]
async fn test_pump_terminates_outbound_line() {
    let (mut pump, _client_tx, mut client_rx) = line_pair();

    pump.send_frame(r#"{"jsonrpc":"2.0","id":1,"method":"x"}"#.into())
        .await
        .expect("send");
    let line = client_rx.recv().await.expect("peer received the frame");
    assert!(line.ends_with('\n'), "one frame is one line: {line}");
    assert!(
        line.contains(r#""method":"x""#),
        "the frame arrived: {line}"
    );
}

/// A response to a registered reverse request wakes its waiter with the
/// full response, and a response nobody waits for is reported false rather
/// than silently swallowed.
#[tokio::test]
async fn test_pump_routes_response() {
    let (mut pump, client_tx, _client_rx) = line_pair();

    let waiter = pump.expect_response(42);
    let resp = encoded(&AcpResponse::ok(
        AcpRequestId::Number(42),
        serde_json::json!({"choice": "allow"}),
    ));
    client_tx
        .send(format!("{resp}\n"))
        .await
        .expect("client sends the response");

    let frame = pump.next_frame().await.expect("response frame");
    match frame {
        InboundFrame::Response(r) => assert!(pump.deliver_response(r)),
        other => panic!("expected a response, got {other:?}"),
    }
    let delivered = waiter.await.expect("waiter received");
    let value = match delivered {
        AcpResponse::Result { result, .. } => result,
        AcpResponse::Error { .. } => panic!("expected a result"),
    };
    assert_eq!(value["choice"], "allow");

    let orphan = AcpResponse::ok(AcpRequestId::Number(99), serde_json::json!({}));
    assert!(!pump.deliver_response(orphan), "no waiter for id 99");
}
