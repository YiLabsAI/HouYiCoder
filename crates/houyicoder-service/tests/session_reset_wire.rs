//! Server-side wire slice for /clear: the frontend SessionReset request
//! zeros the runner's cumulative usage + audit trajectory. This test
//! drives the InProc frontend server with a SessionReset request and
//! asserts the Ack round-trips for the matching session id, and an
//! Error for a mismatched id (the fail-closed guard).

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::{ClientFrame, RequestEnvelope, RequestId};
use houyicoder_protocol::frontend::{FrontendRequest, SessionId as WireSessionId};
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

async fn drain_until(rx: &mut mpsc::Receiver<String>, needle: &str) -> String {
    for _ in 0..32 {
        let line = rx
            .next()
            .await
            .expect("server frame")
            .trim_end()
            .to_string();
        if line.contains(needle) {
            return line;
        }
    }
    String::new()
}

/// A SessionReset for the matching session id returns an Ack — proving the
/// reset round-trips end-to-end on the server side.
#[tokio::test]
async fn test_session_reset_acks_matching() {
    let (runner, session) = stub_runner();
    let session_id = WireSessionId(session.to_string());
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    send(&mut client_tx, &Hello::local()).await;
    drop(client_rx.next().await); // server Hello

    let req = RequestEnvelope::new(RequestId(11), FrontendRequest::SessionReset { session_id });
    send(&mut client_tx, &ClientFrame::Request(req)).await;
    let got = drain_until(&mut client_rx, r#""req_id":11"#).await;
    assert!(!got.is_empty(), "no SessionReset response: {got}");
    assert!(got.contains(r#""type":"ack""#), "expected ack: {got}");

    drop(client_tx);
    drop(handle.await);
}

/// A SessionReset for a mismatched session id fails closed with an Error,
/// not an Ack — a reset must never touch another session's tallies.
#[tokio::test]
async fn test_reset_rejects_mismatched_session() {
    let (runner, session) = stub_runner();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    send(&mut client_tx, &Hello::local()).await;
    drop(client_rx.next().await); // server Hello

    let req = RequestEnvelope::new(
        RequestId(12),
        FrontendRequest::SessionReset {
            session_id: WireSessionId("not-the-real-session".into()),
        },
    );
    send(&mut client_tx, &ClientFrame::Request(req)).await;
    let got = drain_until(&mut client_rx, r#""req_id":12"#).await;
    assert!(!got.is_empty(), "no response: {got}");
    assert!(got.contains(r#""type":"error""#), "expected error: {got}");

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
