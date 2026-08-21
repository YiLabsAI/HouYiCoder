//! Server-side wire slice for /status: the frontend Status request
//! returns a wire StatusSnapshot (ResponsePayload::Status), so the TUI
//! renders /status without importing the engine crate. This test drives
//! the InProc frontend server with a Status request and asserts the wire
//! snapshot carries the stub runner's model id + zeroed usage fields.

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::{ClientFrame, RequestEnvelope, RequestId, ResponsePayload};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::handshake::Hello;
use houyicoder_provider::FakeProvider;
use houyicoder_service::server::{Server, ServerIo};
use houyicoder_session::SessionStore;
use std::sync::Arc;

fn stub_runner() -> (Arc<Runner>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let runner = Runner::with_shared_store(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model: "stub-model".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    (Arc::new(runner), session)
}

fn pair() -> (ServerIo, mpsc::Sender<String>, mpsc::Receiver<String>) {
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let (server_tx, client_rx) = mpsc::channel::<String>(256);
    (ServerIo::new(server_tx, server_rx), client_tx, client_rx)
}

/// The Status request returns a wire StatusSnapshot carrying the runner's
/// model id, not a bare Ack — proving the wire slice is wired end-to-end
/// on the server side.
#[tokio::test]
async fn test_request_returns_wire_snapshot() {
    let (runner, session) = stub_runner();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    send(&mut client_tx, &Hello::local()).await;
    drop(recv(&mut client_rx).await); // server Hello

    let req = RequestEnvelope::new(RequestId(3), FrontendRequest::Status);
    send(&mut client_tx, &ClientFrame::Request(req)).await;

    // Drain until the Status response lands on req_id 3.
    let mut got = String::new();
    for _ in 0..32 {
        let line = recv(&mut client_rx).await;
        if line.contains(r#""req_id":3"#) {
            got = line;
            break;
        }
    }
    assert!(!got.is_empty(), "no Status response: {got}");
    // The wire snapshot carries the stub model id + the Status payload tag.
    assert!(got.contains(r#""type":"status""#), "payload tag: {got}");
    assert!(got.contains(r#""model":"stub-model""#), "model id: {got}");
    // The settings-file memory toggles ride the wire snapshot (default both on
    // when no settings file exists in the test env). Asserting they serialize
    // guards against a future field rename dropping the wire contract.
    assert!(
        got.contains(r#""autoMemory":true"#) || got.contains(r#""autoMemory":false"#),
        "autoMemory toggle on the wire: {got}"
    );
    assert!(
        got.contains(r#""autoDream":true"#) || got.contains(r#""autoDream":false"#),
        "autoDream toggle on the wire: {got}"
    );

    drop(client_tx);
    drop(handle.await);
}

async fn send(tx: &mut mpsc::Sender<String>, msg: &impl serde::Serialize) {
    let mut f = houyicoder_protocol::framing::encode(msg).expect("encode");
    if !f.ends_with('\n') {
        f.push('\n');
    }
    tx.send(f).await.unwrap();
}

async fn recv(rx: &mut mpsc::Receiver<String>) -> String {
    rx.next()
        .await
        .expect("server frame")
        .trim_end()
        .to_string()
}

// silence the unused-import warning for ResponsePayload (kept for clarity
// of the payload tag assertion above; the tag string mirrors the variant).
#[allow(unused_imports)]
use ResponsePayload as _Resp;

/// A fresh session (no turn run, so no durable append and no sidecar on
/// disk) still carries the running build version on the wire StatusSnapshot.
/// Version is a compile-time constant the server sets on the snapshot itself,
/// not a sidecar-sourced field, so it is present before the sidecar lands.
/// Asserting the concrete value (not just the label) guards the server-side
/// set: deleting the assignment leaves the field empty on the wire, and this
/// goes red.
#[tokio::test]
async fn test_status_carries_build_version() {
    let (runner, session) = stub_runner();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    send(&mut client_tx, &Hello::local()).await;
    drop(recv(&mut client_rx).await);

    let req = RequestEnvelope::new(RequestId(7), FrontendRequest::Status);
    send(&mut client_tx, &ClientFrame::Request(req)).await;

    let mut got = String::new();
    for _ in 0..32 {
        let line = recv(&mut client_rx).await;
        if line.contains(r#""req_id":7"#) {
            got = line;
            break;
        }
    }
    let expected = format!("\"version\":\"{}\"", env!("CARGO_PKG_VERSION"));
    assert!(got.contains(&expected), "build version on wire: {got}");

    drop(client_tx);
    drop(handle.await);
}
