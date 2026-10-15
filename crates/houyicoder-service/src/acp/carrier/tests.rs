//! Tests for this module.

use super::*;
use crate::lifecycle::SessionLeaseStore;
use houyicoder_protocol::acp_wire::AcpRequest;
use houyicoder_protocol::acpx::AcpxCapabilities;

fn channel() -> (AcpIo, mpsc::Sender<String>, mpsc::Receiver<String>) {
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    (AcpIo::new(server_tx, server_rx), client_tx, client_rx)
}

#[tokio::test]
async fn test_serve_replies_to_initialize() {
    let adapter = AcpAdapter::new(AcpxCapabilities::default(), 1, SessionLeaseStore::new());
    let (mut io, mut client_tx, mut client_rx) = channel();
    let req = AcpRequest::new(1, "initialize", serde_json::json!({}));
    client_tx
        .send(serde_json::to_string(&req).unwrap() + "\n")
        .await
        .unwrap();
    drop(client_tx);
    drop(serve(&adapter, &mut io).await);
    let reply = client_rx.next().await.expect("reply");
    assert!(reply.contains(r#""id":1"#), "{reply}");
    assert!(reply.contains(r#""protocolVersion":1"#), "{reply}");
}

#[tokio::test]
async fn test_drops_notification_without_reply() {
    let adapter = AcpAdapter::new(AcpxCapabilities::default(), 1, SessionLeaseStore::new());
    let (mut io, mut client_tx, mut client_rx) = channel();
    let notif = AcpNotification::new("session/cancel", serde_json::json!({"sessionId": "01BXHY"}));
    client_tx
        .send(serde_json::to_string(&notif).unwrap() + "\n")
        .await
        .unwrap();
    drop(client_tx);
    drop(serve(&adapter, &mut io).await);
    // Drop the server end so client_rx's sender closes; otherwise
    // client_rx.next() would block forever waiting for a frame that
    // never comes (a notification has no reply).
    drop(io);
    assert!(
        client_rx.next().await.is_none(),
        "notification must not reply"
    );
}

#[tokio::test]
async fn test_serve_replies_parse_error() {
    let adapter = AcpAdapter::new(AcpxCapabilities::default(), 1, SessionLeaseStore::new());
    let (mut io, mut client_tx, mut client_rx) = channel();
    client_tx.send("not json at all\n".into()).await.unwrap();
    drop(client_tx);
    drop(serve(&adapter, &mut io).await);
    let reply = client_rx.next().await.expect("parse-error reply");
    assert!(reply.contains(r#""id":null"#), "{reply}");
    assert!(reply.contains(r#""code":-32700"#), "{reply}");
}
