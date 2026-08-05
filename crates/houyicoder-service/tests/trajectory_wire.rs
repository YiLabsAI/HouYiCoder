//! Server-side wire slice for /trajectory: the frontend Trajectory request
//! returns the session's turn events as a wire Vec<TrajectoryEntry>
//! (ResponsePayload::Trajectory), so the TUI renders the audit log without
//! importing the engine or context crate. Each entry carries the kind label,
//! the ts, the event id, and the prev_hash linking it into the chain. This
//! test drives the InProc frontend server with a Trajectory request after a
//! run and asserts the wire trajectory carries the user + agent entries with
//! their audit fields.

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::{ClientFrame, RequestEnvelope, RequestId};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::{FrontendRequest, SessionId as WireSessionId};
use houyicoder_protocol::handshake::Hello;
use houyicoder_provider::FakeProvider;
use houyicoder_service::server::{Server, ServerIo};
use houyicoder_session::SessionStore;
use std::sync::Arc;

fn stub_runner() -> (Arc<Runner>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("hello from stub"));
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

fn pair() -> (ServerIo, mpsc::Sender<String>, mpsc::Receiver<String>) {
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let (server_tx, client_rx) = mpsc::channel::<String>(256);
    (ServerIo::new(server_tx, server_rx), client_tx, client_rx)
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

/// After a run, the Trajectory request returns the wire turn-event stream
/// (the user message chunk + the agent message chunk the run produced),
/// proving the server projects the engine trajectory to the wire form.
#[tokio::test]
async fn test_returns_wire_session_updates() {
    let (runner, session) = stub_runner();
    let session_str = session.to_string();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    send(&mut client_tx, &Hello::local()).await;
    drop(recv(&mut client_rx).await);

    // Drive one turn so the trajectory has events.
    let req = RequestEnvelope::new(
        RequestId(1),
        FrontendRequest::MessageSend {
            session_id: WireSessionId::new(session_str.clone()),
            content: vec![ContentBlock::Text { text: "hi".into() }],
        },
    );
    send(&mut client_tx, &ClientFrame::Request(req)).await;
    // Drain until the run outcome lands on req_id 1.
    for _ in 0..64 {
        let line = recv(&mut client_rx).await;
        if line.contains(r#""req_id":1"#) {
            break;
        }
    }

    // Now ask for the trajectory.
    let traj = RequestEnvelope::new(RequestId(2), FrontendRequest::Trajectory);
    send(&mut client_tx, &ClientFrame::Request(traj)).await;
    let mut got = String::new();
    for _ in 0..32 {
        let line = recv(&mut client_rx).await;
        if line.contains(r#""req_id":2"#) {
            got = line;
            break;
        }
    }
    assert!(!got.is_empty(), "no Trajectory response: {got}");
    assert!(got.contains(r#""type":"trajectory""#), "payload tag: {got}");
    // The audit log carries the user + assistant entries the run produced,
    // each with the kind label, ts, event_id, and prev_hash audit fields.
    assert!(got.contains("\"kind\":\"user\""), "user entry: {got}");
    assert!(
        got.contains("\"kind\":\"assistant\""),
        "assistant entry: {got}"
    );
    assert!(got.contains("\"eventId\""), "audit event_id field: {got}");
    assert!(got.contains("\"ts\""), "audit ts field: {got}");

    drop(client_tx);
    drop(handle.await);
}
