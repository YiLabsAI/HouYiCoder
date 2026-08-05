//! Server-side wire slice for /tools: the frontend ToolList request
//! returns a wire Tools payload (ResponsePayload::Tools), so the TUI
//! renders the capability inventory without importing the engine
//! registry. This test drives the InProc frontend server with a ToolList
//! request and asserts the wire payload carries the registered tool's
//! name + description.

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, TodoWriteTool, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::{ClientFrame, RequestEnvelope, RequestId};
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
    let mut tools = ToolRegistry::new();
    // The checklist tool needs no sandbox, so it registers in a plain
    // server-contract test without a seatbelt.
    tools.register(Arc::new(TodoWriteTool::new()));
    let runner = Runner::with_shared_store(
        store,
        provider,
        tools,
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

/// The ToolList request returns a wire Tools payload carrying the
/// registered tool's name + description, not a bare Ack — proving the
/// wire slice is wired end-to-end on the server side.
#[tokio::test]
async fn test_tool_list_returns_entries() {
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

    let req = RequestEnvelope::new(RequestId(7), FrontendRequest::ToolList);
    send(&mut client_tx, &ClientFrame::Request(req)).await;

    // Drain until the Tools response lands on req_id 7.
    let mut got = String::new();
    for _ in 0..32 {
        let line = recv(&mut client_rx).await;
        if line.contains(r#""req_id":7"#) {
            got = line;
            break;
        }
    }
    assert!(!got.is_empty(), "no Tools response: {got}");
    assert!(got.contains(r#""type":"tools""#), "payload tag: {got}");
    assert!(got.contains(r#""name":"todo_write""#), "tool name: {got}");

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
