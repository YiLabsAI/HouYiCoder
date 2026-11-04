//! The notification-consumption turn's dispatch contract: a queued ending
//! drives a real turn over the wire, a foreign session id is a typed request
//! error, and an empty queue answers without a provider round trip.

#![cfg(test)]

use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_context::NotificationTopic;
use houyicoder_context::SessionId as EngineSessionId;
use houyicoder_core::agent::multi_agent::bus_types::AgentBus;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::{
    ClientFrame, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::error::ErrorCategory;
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::SessionId;
use houyicoder_protocol::handshake::Hello;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

use super::super::Server;
use super::super::frame_carrier::FrameCarrier;

fn send_line(tx: &mut mpsc::Sender<String>, frame: &impl serde::Serialize) {
    let mut s = encode(frame).expect("encode");
    if !s.ends_with('\n') {
        s.push('\n');
    }
    tx.try_send(s).expect("send frame");
}

async fn recv_frame(rx: &mut mpsc::Receiver<String>) -> ServerFrame {
    serde_json::from_str(&rx.next().await.expect("frame arrives")).expect("frame decodes")
}

fn server_with_session(session: EngineSessionId) -> (Server, Arc<Runner>) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Arc::new(Runner::new(
        store,
        Arc::new(FakeProvider::text("noted")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    ));
    let bus = Arc::new(AgentBus::new());
    let server = Server::new(
        Arc::clone(&runner),
        session,
        Arc::new(DefaultModeGate::new()),
    )
    .with_bus(Some(bus));
    (server, runner)
}

/// A queued ending drives a real turn: the request gets the RunOk reply a
/// user message gets, and the turn's outcome is the provider's answer to the
/// drained material.
#[tokio::test]
async fn test_drain_runs_notification_turn() {
    let session = EngineSessionId::new();
    let (server, runner) = server_with_session(session);
    runner.enqueue_notification(
        "child-a".to_string(),
        NotificationTopic::ChildCompletion,
        "the child finished".to_string(),
        None,
    );
    let (server_tx, mut client_rx) = mpsc::channel::<String>(16);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(16);
    let io = FrameCarrier::new(server_tx, server_rx);
    let handle = tokio::spawn(async move { drop(server.serve(io).await) });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);

    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(7),
            FrontendRequest::NotificationDrain {
                session_id: SessionId(session.to_string()),
                disabled_skills: Default::default(),
            },
        )),
    );
    let mut outcome = None;
    for _ in 0..16 {
        if let ServerFrame::Response(resp) = recv_frame(&mut client_rx).await {
            assert_eq!(resp.req_id, RequestId(7));
            outcome = Some(resp.payload);
            break;
        }
    }
    handle.abort();
    assert!(
        matches!(outcome, Some(ResponsePayload::RunOk(_))),
        "the drained turn answers as a run: {outcome:?}"
    );
}

/// A foreign session id is a typed request error, not a turn on the wrong
/// session: the id gates the drain the same way it gates a user message.
#[tokio::test]
async fn test_drain_rejects_foreign_session() {
    let session = EngineSessionId::new();
    let (server, runner) = server_with_session(session);
    runner.enqueue_notification(
        "child-a".to_string(),
        NotificationTopic::ChildCompletion,
        "the child finished".to_string(),
        None,
    );
    let (server_tx, mut client_rx) = mpsc::channel::<String>(16);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(16);
    let io = FrameCarrier::new(server_tx, server_rx);
    let handle = tokio::spawn(async move { drop(server.serve(io).await) });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);

    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(8),
            FrontendRequest::NotificationDrain {
                session_id: SessionId("another-session".to_string()),
                disabled_skills: Default::default(),
            },
        )),
    );
    let mut reply = None;
    for _ in 0..8 {
        if let ServerFrame::Response(resp) = recv_frame(&mut client_rx).await {
            reply = Some(resp);
            break;
        }
    }
    handle.abort();
    let resp = reply.expect("the mismatch answers");
    assert_eq!(resp.req_id, RequestId(8));
    assert!(
        matches!(&resp.payload, ResponsePayload::Error(e) if e.category == ErrorCategory::InvalidRequest),
        "a foreign session id is a request error: {:?}",
        resp.payload
    );
}

/// An empty queue answers without a provider round trip: the wake signal
/// must not be able to provoke a paid no-op turn.
#[tokio::test]
async fn test_drain_empty_queue_answers() {
    let session = EngineSessionId::new();
    let (server, _runner) = server_with_session(session);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(16);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(16);
    let io = FrameCarrier::new(server_tx, server_rx);
    let handle = tokio::spawn(async move { drop(server.serve(io).await) });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);

    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(9),
            FrontendRequest::NotificationDrain {
                session_id: SessionId(session.to_string()),
                disabled_skills: Default::default(),
            },
        )),
    );
    let mut outcome = None;
    if let Ok(ServerFrame::Response(resp)) =
        timeout(Duration::from_secs(5), recv_frame(&mut client_rx)).await
    {
        assert_eq!(resp.req_id, RequestId(9));
        outcome = Some(resp.payload);
    }
    handle.abort();
    assert!(
        matches!(outcome, Some(ResponsePayload::RunOk(_))),
        "an empty queue is an immediate clean answer: {outcome:?}"
    );
}
