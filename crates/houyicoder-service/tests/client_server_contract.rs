//! Client-server contract: a real frontend client talks to the service
//! server over the in-memory carrier, with the real runner driving the run.
//! The composition root allocates one channel pair and hands the halves to
//! both ends, so client and server share the connection. This is the mode-A
//! path the TUI will hold: the client sends a MessageSend, the server drives
//! the runner, the client receives the turn events on the seq stream and the
//! run outcome as a paired response.

use std::sync::Arc;

use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_client::{Client, InProcTransport};
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::{RequestId, ResponsePayload, ServerFrame};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::run::{ContentBlock, RunOutcome};
use houyicoder_provider::FakeProvider;
use houyicoder_service::server::{Server, ServerIo};
use houyicoder_session::SessionStore;

fn stub_runner() -> (Arc<Runner>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("wire reply"));
    let tools = ToolRegistry::new();
    let runner = Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    (Arc::new(runner), session)
}

/// A full turn over the real wire: the client connects, sends a message, and
/// receives the turn events plus the run outcome. Asserts the outcome pairs
/// to the request id and carries the stub reply.
#[tokio::test]
async fn test_drives_turn_over_wire() {
    let (runner, session) = stub_runner();
    // Allocate the pair here so both ends share it; no second allocation.
    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    let server_io = ServerIo::new(server_tx, server_rx);
    let client_transport = InProcTransport::from_halves(client_tx, client_rx);
    let mut client = Client::new(Box::new(client_transport));
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    client.connect().await.expect("handshake");

    let req_id = RequestId(101);
    client
        .send_request(
            req_id,
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "hello".to_string(),
                }],
            },
        )
        .await
        .expect("send message");

    let mut saw_event = false;
    let mut outcome = None;
    for _ in 0..32 {
        match client.next_frame().await.expect("server frame") {
            ServerFrame::Event(_) => saw_event = true,
            ServerFrame::Response(resp) => {
                assert_eq!(resp.req_id, req_id, "outcome pairs to the request");
                match resp.payload {
                    ResponsePayload::RunOk(run) => {
                        outcome = Some(run.outcome);
                        break;
                    }
                    other => panic!("expected RunOk, got {other:?}"),
                }
            }
            _ => panic!("unexpected frame"),
        }
    }
    drop(client);
    drop(handle.await);
    assert!(saw_event, "client received turn events");
    match outcome.expect("got an outcome") {
        RunOutcome::FinalOutput { content } => {
            let text = content
                .into_iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text),
                    _ => None,
                })
                .expect("a text block");
            assert_eq!(text, "wire reply");
        }
        other => panic!("expected FinalOutput, got {other:?}"),
    }
}

/// The resume cursor advances past each event the client processes, so a
/// reconnect could report the last seq.
#[tokio::test]
async fn test_resume_cursor_advances_events() {
    let (runner, session) = stub_runner();
    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    let server_io = ServerIo::new(server_tx, server_rx);
    let client_transport = InProcTransport::from_halves(client_tx, client_rx);
    let mut client = Client::new(Box::new(client_transport));
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    client.connect().await.expect("handshake");
    let before = client.resume_cursor();
    assert_eq!(before.0, None, "fresh client has no processed seq");

    client
        .send_request(
            RequestId(1),
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "hi".to_string(),
                }],
            },
        )
        .await
        .expect("send");

    // Drain until the outcome arrives; the cursor advances past each event.
    for _ in 0..32 {
        let frame = client.next_frame().await.expect("frame");
        if let ServerFrame::Response(resp) = frame
            && matches!(resp.payload, ResponsePayload::RunOk(_))
        {
            break;
        }
    }
    let after = client.resume_cursor();
    assert!(after.0.is_some(), "cursor advanced past an event");
    drop(client);
    drop(handle.await);
}
