//! The carrier glue for the ACP adapter: read NDJSON lines from a client,
//! dispatch each frame to the adapter, and write the reply back. The adapter
//! owns routing plus typed handlers and is pure of IO; this module owns the
//! line loop and the parse-or-reply boundary. A frame carrying an id field is
//! a JSON-RPC request (the adapter replies on the same id); a frame without
//! one is a notification (no reply). A frame that fails to parse either shape
//! gets a ParseError response on the null id, per JSON-RPC 2.0.
//!
//! The carrier is futures mpsc String pairs, mirroring the frontend ServerIo
//! — one half the composition root hands to the client, the other here. Each
//! frame is one NDJSON line; this module strips and appends the newline
//! terminator so the wire bytes match a pipe.

#![allow(dead_code)] // refactor leftover; the standalone serve fn was superseded by AcpServer and should be removed

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_protocol::acp_wire::{
    AcpError, AcpErrorCode, AcpNotification, AcpRequest, AcpRequestId, AcpResponse, JsonRpcVersion,
};

use crate::acp_adapter::AcpAdapter;

/// The raw frame I/O for the ACP carrier (in-memory mode A). One half of the
/// channel pair the composition root mints; the other half goes to the
/// client. Each frame is a complete NDJSON line (newline included).
pub struct AcpIo {
    tx: mpsc::Sender<String>,
    rx: mpsc::Receiver<String>,
}

impl AcpIo {
    /// Build from the channel halves the composition root allocates.
    pub fn new(tx: mpsc::Sender<String>, rx: mpsc::Receiver<String>) -> Self {
        Self { tx, rx }
    }

    /// Read the next inbound frame, stripping the trailing newline. None
    /// means the client cleanly half-closed its send side.
    pub async fn next_frame(&mut self) -> Option<String> {
        let frame = self.rx.next().await?;
        Some(frame.strip_suffix('\n').unwrap_or(&frame).to_string())
    }

    /// Send one outbound frame, appending the newline terminator. A broken
    /// sender means the client is gone.
    pub async fn send_frame(&mut self, frame: String) -> Result<(), String> {
        let line = if frame.ends_with('\n') {
            frame
        } else {
            format!("{frame}\n")
        };
        self.tx.send(line).await.map_err(|_| "client closed".into())
    }
}

/// Run the connection: receive frames until the client closes. Each frame is
/// parsed as a request (has id) or a notification (no id); the adapter handles
/// it and the reply, if any, is written back. A parse failure replies with a
/// ParseError on the null id. Returns Ok for a clean close, Err for a
/// carrier-level failure the host surfaces.
pub async fn serve(adapter: &AcpAdapter, io: &mut AcpIo) -> Result<(), String> {
    loop {
        let Some(frame) = io.next_frame().await else {
            return Ok(());
        };
        // A request carries an id; a notification does not. Try request first:
        // a notification-shaped frame fails AcpRequest's required-id
        // deserialize and falls through to the notification parse.
        match serde_json::from_str::<AcpRequest>(&frame) {
            Ok(req) => {
                let resp = adapter.handle(&req).await;
                let line = serde_json::to_string(&resp).map_err(|e| e.to_string())?;
                io.send_frame(line).await?;
            }
            Err(_) => match serde_json::from_str::<AcpNotification>(&frame) {
                Ok(notif) => {
                    adapter.handle_notification(&notif).await;
                }
                Err(_) => {
                    // Neither shape parses: JSON-RPC 2.0 replies with a
                    // ParseError on the null id so the peer learns the frame
                    // was rejected (no correlation id to echo).
                    let resp = AcpResponse::Error {
                        jsonrpc: JsonRpcVersion::V2,
                        id: AcpRequestId::Null,
                        error: AcpError {
                            code: AcpErrorCode::ParseError,
                            message: "frame did not parse as request or notification".into(),
                            data: None,
                        },
                    };
                    let line = serde_json::to_string(&resp).map_err(|e| e.to_string())?;
                    io.send_frame(line).await?;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
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
        let notif =
            AcpNotification::new("session/cancel", serde_json::json!({"sessionId": "01BXHY"}));
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
}
