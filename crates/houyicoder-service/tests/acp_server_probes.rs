//! Regression tests baking the adversarial-verify charter's routing-level
//! probes into the gate. The verify agent drove these against the stdio
//! binary by hand; here they run against AcpServer over an in-memory mpsc
//! pair so a future regression fails CI, not a re-verify. Each probe is
//! off the happy path — a malformed frame, a bad method, a missing field —
//! the routing-level behavior that must hold regardless of session binding.

#![cfg(feature = "acp-cross-decode")]

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::acp_wire::AcpRequest;
use houyicoder_protocol::acpx::AcpxCapabilities;
use houyicoder_protocol::framing::encode;
use houyicoder_provider::FakeProvider;
use houyicoder_service::acp_adapter::AcpAdapter;
use houyicoder_service::acp_serve::AcpIo;
use houyicoder_service::acp_server::AcpServer;
use houyicoder_service::lifecycle::SessionLeaseStore;
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
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    (Arc::new(runner), session)
}

fn pair() -> (AcpIo, mpsc::Sender<String>, mpsc::Receiver<String>) {
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let (server_tx, client_rx) = mpsc::channel::<String>(256);
    (AcpIo::new(server_tx, server_rx), client_tx, client_rx)
}

async fn send(tx: &mut mpsc::Sender<String>, msg: &impl serde::Serialize) {
    let mut f = encode(msg).expect("encode");
    if !f.ends_with('\n') {
        f.push('\n');
    }
    tx.send(f).await.unwrap();
}

async fn recv(rx: &mut mpsc::Receiver<String>) -> String {
    rx.next()
        .await
        .expect("server replied")
        .trim_end()
        .to_string()
}

fn server(adapter: Arc<AcpAdapter>, runner: Arc<Runner>, session: SessionId) -> AcpServer {
    AcpServer::new(adapter, runner, session)
}

/// Unknown method replies method-not-found (-32601) on the request id, not
/// null id.
#[tokio::test]
async fn test_unknown_method_not_found() {
    let (runner, session) = stub_runner();
    let adapter = Arc::new(AcpAdapter::new(
        AcpxCapabilities::default(),
        1,
        SessionLeaseStore::new(),
    ));
    let (mut io, mut tx, mut rx) = pair();
    let srv = server(adapter, runner, session);
    let h = tokio::spawn(async move { srv.serve(&mut io).await });
    send(
        &mut tx,
        &AcpRequest::new(99, "bogus", serde_json::json!({})),
    )
    .await;
    let reply = recv(&mut rx).await;
    assert!(reply.contains(r#""id":99"#), "{reply}");
    assert!(reply.contains(r#""code":-32601"#), "{reply}");
    drop(tx);
    drop(h.await);
}

/// Malformed JSON (neither request nor notification) replies ParseError
/// (-32700) on the null id.
#[tokio::test]
async fn test_malformed_frame_parse_error() {
    let (runner, session) = stub_runner();
    let adapter = Arc::new(AcpAdapter::new(
        AcpxCapabilities::default(),
        1,
        SessionLeaseStore::new(),
    ));
    let (mut io, mut tx, mut rx) = pair();
    let srv = server(adapter, runner, session);
    let h = tokio::spawn(async move { srv.serve(&mut io).await });
    tx.send("not json at all\n".into()).await.unwrap();
    let reply = recv(&mut rx).await;
    assert!(reply.contains(r#""id":null"#), "{reply}");
    assert!(reply.contains(r#""code":-32700"#), "{reply}");
    drop(tx);
    drop(h.await);
}

/// session/prompt with no params replies InvalidParams (-32602) on the id.
#[tokio::test]
async fn test_prompt_missing_params_invalid() {
    let (runner, session) = stub_runner();
    let adapter = Arc::new(AcpAdapter::new(
        AcpxCapabilities::default(),
        1,
        SessionLeaseStore::new(),
    ));
    let (mut io, mut tx, mut rx) = pair();
    let srv = server(adapter, runner, session);
    let h = tokio::spawn(async move { srv.serve(&mut io).await });
    // No params field.
    send(
        &mut tx,
        &AcpRequest::new(2, "session/prompt", serde_json::json!({})),
    )
    .await;
    let reply = recv(&mut rx).await;
    assert!(reply.contains(r#""id":2"#), "{reply}");
    assert!(reply.contains(r#""code":-32602"#), "{reply}");
    drop(tx);
    drop(h.await);
}

/// A string request id is echoed back in the reply (a stock ACP client may
/// issue string ids).
#[tokio::test]
async fn test_string_request_id_echoed() {
    use houyicoder_protocol::acp_wire::{AcpRequestId, JsonRpcVersion};
    let (runner, session) = stub_runner();
    let adapter = Arc::new(AcpAdapter::new(
        AcpxCapabilities::default(),
        1,
        SessionLeaseStore::new(),
    ));
    let (mut io, mut tx, mut rx) = pair();
    let srv = server(adapter, runner, session);
    let h = tokio::spawn(async move { srv.serve(&mut io).await });
    let req = AcpRequest {
        jsonrpc: JsonRpcVersion::V2,
        id: AcpRequestId::Str("abc".into()),
        method: "initialize".into(),
        params: None,
    };
    send(&mut tx, &req).await;
    let reply = recv(&mut rx).await;
    assert!(reply.contains(r#""id":"abc""#), "{reply}");
    drop(tx);
    drop(h.await);
}
