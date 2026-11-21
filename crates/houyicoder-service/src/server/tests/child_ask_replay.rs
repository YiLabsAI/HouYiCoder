//! Child permission bus subscription and reverse-request frame conservation.
//!
//! One connection holds one permission receiver across idle waits and active
//! runs so an ask answered mid-run is never replayed when the run returns to
//! the idle loop. A client request arriving on the full-duplex carrier while a
//! reverse permission ask awaits its answer is deferred and dispatched once the
//! ask settles instead of being dropped without a response.

#![cfg(test)]

use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_async::bus::MessageBus;
use houyicoder_context::NotificationTopic;
use houyicoder_context::SessionId as EngineSessionId;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, permission_request_topic, permission_response_topic,
};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponseEnvelope, ClientResponsePayload, RequestEnvelope, RequestId,
    ResponsePayload, ServerFrame, ServerRequestEnvelope, ServerRequestPayload,
};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::SessionId;
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalScope};
use houyicoder_protocol::handshake::Hello;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem};
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{sleep, timeout};

use super::super::Server;
use super::super::frame_carrier::FrameCarrier;

fn send_line(tx: &mut mpsc::Sender<String>, frame: &impl serde::Serialize) {
    let mut s = encode(frame).expect("encode");
    if !s.ends_with('\n') {
        s.push('\n');
    }
    tx.try_send(s).expect("send frame");
}

fn send_drain(tx: &mut mpsc::Sender<String>, id: u64, session: EngineSessionId) {
    send_line(
        tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(id),
            FrontendRequest::NotificationDrain {
                session_id: SessionId(session.to_string()),
                disabled_skills: Default::default(),
            },
        )),
    );
}

fn approve_once(tx: &mut mpsc::Sender<String>, req_id: RequestId, call_id: &str, approved: bool) {
    send_line(
        tx,
        &ClientFrame::Response(ClientResponseEnvelope::new(
            req_id,
            ClientResponsePayload::Permission(ApprovalDecision {
                call_id: call_id.into(),
                approved,
                updated_input: None,
                scope: ApprovalScope::Once,
            }),
        )),
    );
}

fn delayed_server(session: EngineSessionId, delay_ms: u64) -> (Server, Arc<Runner>, Arc<AgentBus>) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider = FakeProvider::new_with_delay(
        vec![
            CompletionResponse {
                output: vec![OutputItem::Text {
                    text: "first".into(),
                }],
                usage: Default::default(),
                model: "test".into(),
            },
            CompletionResponse {
                output: vec![OutputItem::Text {
                    text: "second".into(),
                }],
                usage: Default::default(),
                model: "test".into(),
            },
        ],
        delay_ms,
    );
    let provider: Arc<dyn ModelProvider> = Arc::new(provider);
    let runner = Arc::new(Runner::new(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    ));
    let bus = Arc::new(AgentBus::new());
    let server = Server::new(
        Arc::clone(&runner),
        session,
        Arc::new(DefaultModeGate::new()),
    )
    .with_bus(Some(Arc::clone(&bus)));
    (server, runner, bus)
}

/// Answering a child permission ask during an active run consumes it once;
/// returning to the between-run serve loop must not replay a second ghost card.
#[tokio::test]
async fn test_child_ask_answered_once() {
    let session = EngineSessionId::new();
    let (server, runner, bus) = delayed_server(session, 150);
    runner.enqueue_notification(
        "child-a".into(),
        NotificationTopic::ChildCompletion,
        "ready".into(),
        None,
    );
    let (server_tx, mut client_rx) = mpsc::channel::<String>(32);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(32);
    let io = FrameCarrier::new(server_tx, server_rx);
    let handle = tokio::spawn(async move { drop(server.serve(io).await) });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);

    // Start an active turn so serve enters drive_while.
    send_drain(&mut client_tx, 1, session);
    sleep(Duration::from_millis(25)).await;

    let mut resp_rx = bus.subscribe(&permission_response_topic("child-a", "call-once"));
    bus.publish(
        permission_request_topic(),
        BusMessage::PermissionRequest {
            child_id: "child-a".into(),
            subagent_type: "general-purpose".into(),
            call_id: "call-once".into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "rm -rf /tmp/probe"}),
            reason: None,
        },
    );

    let mut cards_seen = 0usize;
    let mut turn_done = false;
    timeout(Duration::from_secs(3), async {
        while !turn_done {
            let line = client_rx.next().await.expect("server frame");
            let frame: ServerFrame = serde_json::from_str(&line).expect("decode");
            match frame {
                ServerFrame::Request(req) => {
                    cards_seen += 1;
                    approve_once(&mut client_tx, req.req_id, "call-once", true);
                }
                ServerFrame::Response(resp) if resp.req_id == RequestId(1) => {
                    assert!(matches!(resp.payload, ResponsePayload::RunOk(_)));
                    turn_done = true;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("first turn settles");
    drop(resp_rx.recv().await);
    assert_eq!(cards_seen, 1, "one card shown during the active turn");

    // Now back in the idle serve loop: send a second request and assert no
    // ghost Permission card arrives ahead of its response.
    send_drain(&mut client_tx, 2, session);
    timeout(Duration::from_secs(2), async {
        loop {
            let line = client_rx.next().await.expect("server frame");
            let frame: ServerFrame = serde_json::from_str(&line).expect("decode");
            match frame {
                ServerFrame::Request(ServerRequestEnvelope {
                    payload: ServerRequestPayload::Permission(ask),
                    ..
                }) => {
                    panic!(
                        "between-run loop replayed an already-answered child ask: {}",
                        ask.call_id
                    );
                }
                ServerFrame::Response(resp) if resp.req_id == RequestId(2) => break,
                _ => {}
            }
        }
    })
    .await
    .expect("second request answered without ghost cards");
    handle.abort();
}

/// A client Request frame that crosses on the wire with a reverse Permission
/// ask must be deferred and answered once the ask settles, never silently
/// dropped (which leaves the frontend RunState stuck in Running forever).
#[tokio::test]
async fn test_request_during_ask_preserved() {
    let session = EngineSessionId::new();
    let (server, runner, bus) = delayed_server(session, 5);
    runner.enqueue_notification(
        "task-54".into(),
        NotificationTopic::ChildCompletion,
        "make check passed".into(),
        None,
    );
    let (server_tx, mut client_rx) = mpsc::channel::<String>(32);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(32);
    let io = FrameCarrier::new(server_tx, server_rx);
    let handle = tokio::spawn(async move { drop(server.serve(io).await) });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);
    sleep(Duration::from_millis(20)).await;

    // Server enters handle_approval waiting for the permission verdict.
    bus.publish(
        permission_request_topic(),
        BusMessage::PermissionRequest {
            child_id: "child-b".into(),
            subagent_type: "general-purpose".into(),
            call_id: "call-race".into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "rm -rf /tmp/probe"}),
            reason: None,
        },
    );

    let ask_id = timeout(Duration::from_secs(2), async {
        loop {
            let line = client_rx.next().await.expect("server frame");
            if let Ok(ServerFrame::Request(req)) = serde_json::from_str(&line) {
                break req.req_id;
            }
        }
    })
    .await
    .expect("permission card arrives");

    // Client auto-wake sends NotificationDrain BEFORE answering the permission
    // card (the full-duplex crossing race).
    send_drain(&mut client_tx, 42, session);
    approve_once(&mut client_tx, ask_id, "call-race", false);

    let drain_reply = timeout(Duration::from_millis(800), async {
        loop {
            let line = client_rx.next().await.expect("server frame");
            if let Ok(ServerFrame::Response(resp)) = serde_json::from_str(&line)
                && resp.req_id == RequestId(42)
            {
                break resp.payload;
            }
        }
    })
    .await
    .expect("concurrent NotificationDrain must receive its Response instead of hanging");
    assert!(matches!(drain_reply, ResponsePayload::RunOk(_)));
    handle.abort();
}

/// A cancel notification arriving back-to-back right behind a run request must
/// observe the run's installed cancel token rather than firing before the run
/// future is first polled.
#[tokio::test]
async fn test_immediate_cancel_aborts_run() {
    let session = EngineSessionId::new();
    let (server, runner, _bus) = delayed_server(session, 2000);
    runner.enqueue_notification(
        "child-c".into(),
        NotificationTopic::ChildCompletion,
        "slow".into(),
        None,
    );
    let (server_tx, mut client_rx) = mpsc::channel::<String>(32);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(32);
    let io = FrameCarrier::new(server_tx, server_rx);
    let handle = tokio::spawn(async move { drop(server.serve(io).await) });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);
    sleep(Duration::from_millis(20)).await;

    send_drain(&mut client_tx, 77, session);
    send_line(
        &mut client_tx,
        &AcpNotification::new("session/cancel", serde_json::json!({})),
    );

    let reply = timeout(Duration::from_millis(800), async {
        loop {
            let line = client_rx.next().await.expect("server frame");
            if let Ok(ServerFrame::Response(resp)) = serde_json::from_str(&line)
                && resp.req_id == RequestId(77)
            {
                break resp.payload;
            }
        }
    })
    .await
    .expect("immediate cancel interrupts the slow run before its 2s provider delay");
    match reply {
        ResponsePayload::RunOk(run) => assert!(
            format!("{run:?}")
                .to_ascii_lowercase()
                .contains("interrupt"),
            "expected interrupted run outcome, got {run:?}"
        ),
        ResponsePayload::RunErr(_) => {}
        other => panic!("expected interrupted run reply, got {other:?}"),
    }
    handle.abort();
}
