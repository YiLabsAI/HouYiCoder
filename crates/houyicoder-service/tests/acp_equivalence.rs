//! Dual-transport semantic equivalence: the InProc frontend server (the
//! envelope dialect, mode-A) and the ACP base server (JSON-RPC, mode-B)
//! both drive the same engine runner, so a turn over either transport
//! must produce the same outcome. This guards against the two server
//! paths diverging — a regression that makes one transport stop_reason
//! differ from the other fails here.

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
use houyicoder_protocol::envelope::{ClientFrame, RequestEnvelope, RequestId, ServerFrame};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::{FrontendRequest, SessionId as WireSessionId};
use houyicoder_protocol::handshake::Hello;
use houyicoder_provider::FakeProvider;
use houyicoder_service::acp_adapter::AcpAdapter;
use houyicoder_service::acp_serve::AcpIo;
use houyicoder_service::acp_server::AcpServer;
use houyicoder_service::lifecycle::SessionLeaseStore;
use houyicoder_service::server::{Server, ServerIo};
use houyicoder_session::SessionStore;
use std::sync::Arc;

const REPLY: &str = "hello from stub";

fn stub_runner() -> (Arc<Runner>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text(REPLY));
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

/// Drive one turn over the InProc frontend envelope transport; return the
/// final stop_reason string the server reported.
async fn frontend_turn_stop_reason(runner: Arc<Runner>, session: SessionId) -> String {
    let (server_tx, client_rx) = mpsc::channel::<String>(256);
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let server_io = ServerIo::new(server_tx, server_rx);
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    let mut tx = client_tx;
    let mut rx = client_rx;
    send(&mut tx, &Hello::local()).await;
    drop(recv(&mut rx).await); // server Hello

    let req = RequestEnvelope::new(
        RequestId(7),
        FrontendRequest::MessageSend {
            session_id: WireSessionId::new(session.to_string()),
            content: vec![ContentBlock::Text { text: "hi".into() }],
        },
    );
    send(&mut tx, &ClientFrame::Request(req)).await;

    // Drain until the run outcome response lands on req_id 7.
    let mut stop = String::new();
    for _ in 0..64 {
        let line = recv(&mut rx).await;
        if line.contains(r#""req_id":7"#) {
            stop = line;
            break;
        }
    }
    drop(tx);
    drop(handle.await);
    stop
}

/// Drive one turn over the ACP base JSON-RPC transport (mpsc, not stdio —
/// the stdio path is covered by acp_drop_in; this isolates the server
/// logic from the carrier).
async fn acp_turn_stop_reason(runner: Arc<Runner>, session: SessionId) -> String {
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let (server_tx, client_rx) = mpsc::channel::<String>(256);
    let mut io = AcpIo::new(server_tx, server_rx);
    let adapter = Arc::new(AcpAdapter::new(
        AcpxCapabilities::default(),
        1,
        SessionLeaseStore::new(),
    ));
    let server = AcpServer::new(adapter, runner, session);
    let handle = tokio::spawn(async move { server.serve(&mut io).await });

    let mut tx = client_tx;
    let mut rx = client_rx;
    send(
        &mut tx,
        &AcpRequest::new(1, "initialize", serde_json::json!({})),
    )
    .await;
    drop(recv(&mut rx).await); // initialize reply

    let prompt = serde_json::json!({
        "sessionId": session.to_string(),
        "prompt": [{"type": "text", "text": "hi"}],
    });
    send(&mut tx, &AcpRequest::new(2, "session/prompt", prompt)).await;

    let mut stop = String::new();
    for _ in 0..64 {
        let line = recv(&mut rx).await;
        if line.contains(r#""id":2"#) {
            stop = line;
            break;
        }
    }
    drop(tx);
    drop(handle.await);
    stop
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
        .expect("server frame")
        .trim_end()
        .to_string()
}

/// Both transports, same stub runner + same "hi" input, produce an
/// end-turn stop reason. The frontend server reports RunOk (stop_reason
/// EndTurn); the ACP server reports PromptResponse stopReason end_turn.
/// Equivalence: both end the turn with the legal end-turn reason, not
/// cancelled/refusal/max-tokens — proving neither transport diverges.
#[tokio::test]
async fn test_transports_end_turn_equivalent() {
    let (runner_a, session_a) = stub_runner();
    let (runner_b, session_b) = stub_runner();

    let fe = frontend_turn_stop_reason(runner_a, session_a).await;
    let acp = acp_turn_stop_reason(runner_b, session_b).await;

    assert!(
        fe.contains(r#""stop_reason":"end_turn""#),
        "frontend stop reason: {fe}"
    );
    assert!(
        acp.contains(r#""stopReason":"end_turn""#),
        "acp stop reason: {acp}"
    );
    // The stub reply lands as an agent message chunk on both transports.
    // (The frontend streams it as a SessionUpdate; the ACP as a
    // session/update notification — both carry the same text.)
}
