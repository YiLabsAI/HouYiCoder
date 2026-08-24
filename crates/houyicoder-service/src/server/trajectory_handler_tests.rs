//! Dispatch handler wiring tests: drive a FrontendRequest through the real
//! Server + store and assert the response. Split out of dispatch.rs so that
//! file stays under the size gate.
use super::*;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_protocol::envelope::{ClientFrame, RequestEnvelope, RequestId};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::handshake::Hello;
use houyicoder_session::SessionStore;
use std::sync::Arc;

fn stub_runner() -> std::sync::Arc<houyicoder_core::agent::Runner> {
    let store = Arc::new(SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    Arc::new(houyicoder_core::agent::Runner::with_shared_store(
        store,
        Arc::new(houyicoder_provider::FakeProvider::text("x")),
        houyicoder_core::agent::ToolRegistry::new(),
        houyicoder_core::agent::runner_config::RunnerConfig {
            model: "test".into(),
            ..houyicoder_core::agent::runner_config::RunnerConfig::default()
        },
    ))
}

/// A Trajectory request drives the dispatch handler (project_trajectory +
/// project_redundant + TrajectoryResponse build). No run, so entries +
/// redundant are empty, but the handler path executes.
#[tokio::test]
async fn test_trajectory_handler_builds_response() {
    let runner = stub_runner();
    let session = houyicoder_context::SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = ServerIo::new(server_tx, server_rx);
    let server = Server::new(
        runner,
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(io).await });
    let mut tx = client_tx.clone();
    let mut f = encode(&Hello::local()).unwrap();
    if !f.ends_with('\n') {
        f.push('\n');
    }
    tx.try_send(f).unwrap();
    drop(client_rx.next().await.unwrap()); // hello ack
    let req = encode(&ClientFrame::Request(RequestEnvelope::new(
        RequestId(1),
        FrontendRequest::Trajectory,
    )))
    .unwrap();
    let mut req = req;
    if !req.ends_with('\n') {
        req.push('\n');
    }
    tx.try_send(req).unwrap();
    let resp = client_rx.next().await.unwrap();
    handle.abort();
    assert!(resp.contains("trajectory"), "response: {resp}");
}

/// A ChildTranscript request replays the child session log and projects each
/// turn event to the same session/update + acpx frame stream the parent
/// accumulates. Seeds a child log with a user input + assistant message,
/// drives the request, and asserts the response carries the projected
/// frames. Covers the emit.rs projection loop the no-log wired TUI test
/// skips.
#[tokio::test]
async fn test_child_transcript_projects_log() {
    use houyicoder_context::{EventId, SessionId, TurnEvent, TurnEventKind};
    let runner = stub_runner();
    let child = SessionId::new();
    for (ts, kind) in [
        (
            0,
            TurnEventKind::UserInput {
                text: "find auth".into(),
            },
        ),
        (
            1,
            TurnEventKind::AssistantMessage {
                text: "auth is in src/auth".into(),
                thinking: None,
            },
        ),
    ] {
        runner
            .store()
            .append(TurnEvent {
                id: EventId::new(),
                session: child,
                ts,
                prev_hash: None,
                kind,
            })
            .await
            .unwrap();
    }
    let session = SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = ServerIo::new(server_tx, server_rx);
    let server = Server::new(
        runner,
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(io).await });
    let mut tx = client_tx.clone();
    let mut f = encode(&Hello::local()).unwrap();
    if !f.ends_with('\n') {
        f.push('\n');
    }
    tx.try_send(f).unwrap();
    drop(client_rx.next().await.unwrap()); // hello ack
    let req = encode(&ClientFrame::Request(RequestEnvelope::new(
        RequestId(2),
        FrontendRequest::ChildTranscript {
            child_sid: houyicoder_protocol::frontend::SessionId(child.to_string()),
        },
    )))
    .unwrap();
    let mut req = req;
    if !req.ends_with('\n') {
        req.push('\n');
    }
    tx.try_send(req).unwrap();
    let resp = client_rx.next().await.unwrap();
    handle.abort();
    assert!(
        resp.contains("child_transcript"),
        "child_transcript response: {resp}"
    );
    assert!(
        resp.contains("find auth"),
        "child user input projected into the response: {resp}"
    );
    assert!(
        resp.contains("auth is in src/auth"),
        "child assistant text projected into the response: {resp}"
    );
}
