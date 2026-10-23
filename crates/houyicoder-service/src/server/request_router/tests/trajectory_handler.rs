//! Router wiring tests: drive a FrontendRequest through the real Server and
//! store, then assert the response.
use super::super::*;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::{ClientFrame, RequestEnvelope, RequestId};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::handshake::Hello;
use houyicoder_protocol::llm::CompletionResponse;
use houyicoder_protocol::llm::OutputItem;
use houyicoder_protocol::llm::Usage;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use std::sync::Arc;

fn stub_runner() -> Arc<Runner> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Arc::new(Runner::new(
        store,
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            ..RunnerConfig::default()
        },
    ))
}

/// A runner whose provider delays before its first event, so a parent run
/// stays in-flight long enough for the test to send a mid-run frame and the
/// serve loop's mid-run select to handle it.
fn delayed_runner(delay_ms: u64) -> Arc<Runner> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let resp = CompletionResponse {
        output: vec![OutputItem::Text { text: "ok".into() }],
        usage: Usage::default(),
        model: "test".into(),
    };
    Arc::new(Runner::new(
        store,
        Arc::new(FakeProvider::new_with_delay(vec![resp], delay_ms)),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            ..RunnerConfig::default()
        },
    ))
}

/// A Trajectory request drives the dispatch handler (build_trajectory_entries +
/// map_redundant_entries + TrajectoryResponse build). No run, so entries +
/// redundant are empty, but the handler path executes.
#[tokio::test]
async fn test_trajectory_handler_builds_response() {
    let runner = stub_runner();
    let session = houyicoder_context::SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let server = Server::new(runner, session, Arc::new(DefaultModeGate::new()));
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

/// A ChildTranscript request replays the child session log and maps each turn
/// event to the same session/update + acpx frame stream the parent
/// accumulates. Seeds a child log with a user input + assistant message,
/// drives the request, and asserts the response carries the mapped frames.
/// Covers the outbound frame mapping the no-log wired TUI test skips.
#[tokio::test]
async fn test_child_transcript_reads_log() {
    let runner = stub_runner();
    let child = SessionId::new();
    for (ts, kind) in [
        (
            0,
            SessionEvent::UserInput {
                text: "find auth".into(),
            },
        ),
        (
            1,
            SessionEvent::AssistantMessage {
                text: "auth is in src/auth".into(),
                thinking: None,
            },
        ),
    ] {
        runner
            .store()
            .append(SessionLogEntry {
                id: EventId::new(),
                session: child,
                ts,
                prev_hash: None,
                event: kind,
            })
            .await
            .unwrap();
    }
    let session = SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let server = Server::new(runner, session, Arc::new(DefaultModeGate::new()));
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
        "child user input mapped into the response: {resp}"
    );
    assert!(
        resp.contains("auth is in src/auth"),
        "child assistant text mapped into the response: {resp}"
    );
}

/// A ChildTranscript request received MID-RUN must be handled, not dropped.
/// The mid-run select arm routes requests to handle_request_during_run; the
/// ChildTranscript payload is a read-only store query so it does not race the
/// parent run. This pins the fix against the prior drop in the mid-run arm,
/// which left the TUI stuck on the placeholder until the run ended.
#[tokio::test]
async fn test_child_transcript_during_run() {
    let runner = stub_runner();
    let child = SessionId::new();
    for (ts, kind) in [
        (
            0,
            SessionEvent::UserInput {
                text: "find auth".into(),
            },
        ),
        (
            1,
            SessionEvent::AssistantMessage {
                text: "auth is in src/auth".into(),
                thinking: None,
            },
        ),
    ] {
        runner
            .store()
            .append(SessionLogEntry {
                id: EventId::new(),
                session: child,
                ts,
                prev_hash: None,
                event: kind,
            })
            .await
            .unwrap();
    }
    let session = SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (_client_tx, server_rx) = mpsc::channel::<String>(256);
    let mut io = FrameCarrier::new(server_tx, server_rx);
    let mut server = Server::new(runner, session, Arc::new(DefaultModeGate::new()));
    // Call the mid-run handler directly (the serve loop routes mid-run
    // requests here); no run is in flight, but the handler is the unit under
    // test — it must replay the child log, map it to frames, and respond.
    server
        .handle_request_during_run(
            &mut io,
            RequestEnvelope::new(
                RequestId(3),
                FrontendRequest::ChildTranscript {
                    child_sid: houyicoder_protocol::frontend::SessionId(child.to_string()),
                },
            ),
        )
        .await;
    let resp = client_rx.next().await.unwrap();
    assert!(
        resp.contains("child_transcript"),
        "mid-run handler responded: {resp}"
    );
    assert!(
        resp.contains("find auth"),
        "mid-run child transcript maps the child log: {resp}"
    );
}

/// A ModelInfo request received MID-RUN must be answered, not dropped. The
/// pane opens from a keystroke the user can press while a turn streams, so a
/// query that only lands between runs leaves it on the empty-state guide for
/// the whole turn and makes a catalog row unpickable exactly when the user
/// wants to switch. Read-only mapping: it does not race the parent run.
#[tokio::test]
async fn test_model_query_during_run() {
    let runner = stub_runner();
    let session = houyicoder_context::SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (_client_tx, server_rx) = mpsc::channel::<String>(256);
    let mut io = FrameCarrier::new(server_tx, server_rx);
    let mut server = Server::new(runner, session, Arc::new(DefaultModeGate::new()));
    server
        .handle_request_during_run(
            &mut io,
            RequestEnvelope::new(RequestId(9), FrontendRequest::ModelInfo),
        )
        .await;
    let resp = client_rx.next().await.unwrap();
    assert!(
        resp.contains("model_info"),
        "the mid-run handler answered the query: {resp}"
    );
    assert!(
        resp.contains("\"test\""),
        "the reply describes the session's live model: {resp}"
    );
}

/// A PermissionCycleMode request received mid-run cycles the gate + ships a
/// PermissionMode response. Pins the other arm of handle_request_during_run,
/// the one the existing integration test exercises through the serve loop but
/// the lib coverage gate does not see.
#[tokio::test]
async fn test_permission_cycle_during_run() {
    let runner = stub_runner();
    let session = houyicoder_context::SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (_client_tx, server_rx) = mpsc::channel::<String>(256);
    let mut io = FrameCarrier::new(server_tx, server_rx);
    let mut server = Server::new(runner, session, Arc::new(DefaultModeGate::new()));
    server
        .handle_request_during_run(
            &mut io,
            RequestEnvelope::new(RequestId(4), FrontendRequest::PermissionCycleMode),
        )
        .await;
    let resp = client_rx.next().await.unwrap();
    assert!(
        resp.contains("permission_mode"),
        "mid-run mode cycle responds with the new mode: {resp}"
    );
}

/// A ChildTranscript request sent WHILE a parent turn is running reaches the
/// serve loop's mid-run select, which routes it to handle_request_during_run
/// and responds — the fix for the prior drop that left the TUI stuck on the
/// placeholder until the run ended. A delayed provider keeps the run
/// in-flight so the mid-run arm fires; the child log is seeded so the
/// response carries the mapped transcript.
#[tokio::test]
async fn test_child_fetch_during_run() {
    let runner = delayed_runner(800);
    let child = SessionId::new();
    runner
        .store()
        .append(SessionLogEntry {
            id: EventId::new(),
            session: child,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "find auth".into(),
            },
        })
        .await
        .unwrap();
    let session = SessionId::new();
    let wire_session = houyicoder_protocol::frontend::SessionId(session.to_string());
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let server = Server::new(runner, session, Arc::new(DefaultModeGate::new()));
    let handle = tokio::spawn(async move { server.serve(io).await });
    let mut tx = client_tx.clone();
    let mut f = encode(&Hello::local()).unwrap();
    if !f.ends_with('\n') {
        f.push('\n');
    }
    tx.try_send(f).unwrap();
    drop(client_rx.next().await.unwrap()); // hello ack
    // Start the run (provider delays 800ms, so the run is in-flight).
    let run_req = encode(&ClientFrame::Request(RequestEnvelope::new(
        RequestId(5),
        FrontendRequest::MessageSend {
            session_id: wire_session,
            content: vec![ContentBlock::Text { text: "go".into() }],
            disabled_skills: Default::default(),
        },
    )))
    .unwrap();
    let mut run_req = run_req;
    if !run_req.ends_with('\n') {
        run_req.push('\n');
    }
    tx.try_send(run_req).unwrap();
    // While the run is in-flight, send the child-transcript fetch.
    let fetch_req = encode(&ClientFrame::Request(RequestEnvelope::new(
        RequestId(6),
        FrontendRequest::ChildTranscript {
            child_sid: houyicoder_protocol::frontend::SessionId(child.to_string()),
        },
    )))
    .unwrap();
    let mut fetch_req = fetch_req;
    if !fetch_req.ends_with('\n') {
        fetch_req.push('\n');
    }
    tx.try_send(fetch_req).unwrap();
    // The mid-run select handles the fetch; the response arrives before the
    // run completes (the 800ms delay is still ongoing). Drain frames until we
    // see the child_transcript response (run frames may interleave).
    let mut saw = false;
    for _ in 0..200 {
        let frame = match client_rx.next().await {
            Some(f) => f,
            None => break,
        };
        if frame.contains("child_transcript") && frame.contains("find auth") {
            saw = true;
            break;
        }
    }
    handle.abort();
    assert!(
        saw,
        "the mid-run fetch must be handled while the parent run is in-flight, \
         not dropped"
    );
}

/// A read-only request is answered while a turn is in flight: the client that
/// asked for the pane must not be left waiting on a response the server never
/// sends.
#[tokio::test]
async fn test_read_only_request_served() {
    let runner = stub_runner();
    let session = houyicoder_context::SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (_client_tx, server_rx) = mpsc::channel::<String>(256);
    let mut io = FrameCarrier::new(server_tx, server_rx);
    let mut server = Server::new(runner, session, Arc::new(DefaultModeGate::new()));
    server
        .handle_request_during_run(
            &mut io,
            RequestEnvelope::new(RequestId(7), FrontendRequest::Context),
        )
        .await;
    let resp = client_rx
        .try_recv()
        .expect("a read-only mid-run request is answered");
    assert!(
        resp.contains("\"req_id\":7"),
        "the answer carries the request id"
    );
    assert!(
        resp.contains("\"type\":\"context\""),
        "the answer is the payload the request asked for"
    );
}

/// Renaming the session is a descriptor write, not a conversation change, so
/// it lands while the turn is in flight. The pane that shows the name must not
/// have to wait for the run to end.
#[tokio::test]
async fn test_rename_served_during_run() {
    let runner = stub_runner();
    let session = houyicoder_context::SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (_client_tx, server_rx) = mpsc::channel::<String>(256);
    let mut io = FrameCarrier::new(server_tx, server_rx);
    let mut server = Server::new(runner, session, Arc::new(DefaultModeGate::new()));
    server
        .handle_request_during_run(
            &mut io,
            RequestEnvelope::new(
                RequestId(9),
                FrontendRequest::RenameSession {
                    session_id: houyicoder_protocol::frontend::SessionId(session.to_string()),
                    name: "renamed mid-run".into(),
                },
            ),
        )
        .await;
    let resp = client_rx.try_recv().expect("a rename is answered");
    assert!(
        !resp.contains("waits for the running turn"),
        "a descriptor write is not refused mid-run: {resp}"
    );
}

/// A cancel request is handled, not deferred: refusing it would refuse the
/// verb exactly when the user needs it.
#[tokio::test]
async fn test_cancel_request_served() {
    let runner = stub_runner();
    let session = houyicoder_context::SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (_client_tx, server_rx) = mpsc::channel::<String>(256);
    let mut io = FrameCarrier::new(server_tx, server_rx);
    let mut server = Server::new(runner, session, Arc::new(DefaultModeGate::new()));
    server
        .handle_request_during_run(
            &mut io,
            RequestEnvelope::new(
                RequestId(10),
                FrontendRequest::RunCancel {
                    session_id: houyicoder_protocol::frontend::SessionId("s".into()),
                    reason: "user asked".into(),
                },
            ),
        )
        .await;
    let resp = client_rx.try_recv().expect("a cancel is answered");
    assert!(
        resp.contains("cancelled"),
        "the answer says the cancel was taken: {resp}"
    );
    assert!(
        !resp.contains("waits for the running turn"),
        "a cancel must not be deferred: {resp}"
    );
}

/// A request that must wait for the turn is refused with an error, not
/// dropped. The client blocks on a response for every request it sends, so a
/// dropped one is indistinguishable from a hung server.
#[tokio::test]
async fn test_mutating_request_refused() {
    let runner = stub_runner();
    let session = houyicoder_context::SessionId::new();
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (_client_tx, server_rx) = mpsc::channel::<String>(256);
    let mut io = FrameCarrier::new(server_tx, server_rx);
    let mut server = Server::new(runner, session, Arc::new(DefaultModeGate::new()));
    server
        .handle_request_during_run(
            &mut io,
            RequestEnvelope::new(
                RequestId(8),
                FrontendRequest::DiffApply {
                    path: "a.rs".into(),
                    patch: "x".into(),
                },
            ),
        )
        .await;
    let resp = client_rx
        .try_recv()
        .expect("a refused mid-run request is answered");
    assert!(
        resp.contains("waits for the running turn"),
        "the refusal says why: {resp}"
    );
}
