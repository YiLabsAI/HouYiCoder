//! Reconnection contracts for interrupted sessions.
//!
//! These tests verify that a new connection restores pending permission work,
//! queued input projection, event order, and lifecycle ownership without
//! reviving terminal sessions or accepting unrelated responses. Disconnects
//! at each recovery boundary must remain resumable and preserve prior choices.

use super::tests::*;
use super::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_client::{Client, InProcTransport};
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::acpx::AcpxMethod;
use houyicoder_protocol::envelope::{
    ClientResponsePayload, EventEnvelope, RequestId, ServerFrame, ServerRequestPayload,
};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalRequest, ContentBlock};
use houyicoder_protocol::frontend::session_update::SessionUpdate;
use houyicoder_protocol::frontend::{FrontendEvent, FrontendRequest, QueuedInput};
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use houyicoder_session::SessionStore;
use serde_json::Value;
use tokio::sync::{Notify, oneshot};

use crate::composition::SessionHost;
use crate::lifecycle::{Lifecycle, LifecycleState, SessionLeaseStore};

/// A read-only tool that keeps a resumed run active until released.
struct ReconnectBlockingTool {
    entered: Arc<Notify>,
    release: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
}

impl Tool for ReconnectBlockingTool {
    fn name(&self) -> &str {
        "reconnect_blocking"
    }

    fn description(&self) -> &str {
        "blocks a resumed run until the test releases it"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }

    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let entered = self.entered.clone();
        let release = self.release.clone();
        Box::pin(async move {
            entered.notify_one();
            let receiver = release.lock().expect("release lock").take();
            if let Some(receiver) = receiver {
                let _ = receiver.await;
            }
            Ok(serde_json::json!({"ok": true}))
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

fn reconnect_user_message(frame: &ServerFrame, text: &str) -> bool {
    matches!(
        frame,
        ServerFrame::Event(EventEnvelope {
            payload: FrontendEvent::SessionUpdate {
                update: SessionUpdate::UserMessageChunk(chunk),
            },
            ..
        }) if matches!(&chunk.content, ContentBlock::Text { text: body } if body == text)
    )
}

fn reconnect_commit(frame: &ServerFrame, input: &QueuedInput) -> bool {
    matches!(
        frame,
        ServerFrame::Event(EventEnvelope {
            payload: FrontendEvent::QueuedInputCommitted { inputs },
            ..
        }) if inputs.iter().any(|committed| committed.id == input.id)
    )
}

fn reconnect_text_delta(frame: &ServerFrame) -> bool {
    matches!(
        frame,
        ServerFrame::Event(EventEnvelope {
            payload: FrontendEvent::Acpx { notification },
            ..
        }) if notification.method == AcpxMethod::LlmTextDelta
    )
}

async fn await_inject_dispatch(client: &mut Client) {
    let req_id = RequestId(u64::MAX - 1);
    client
        .send_request(
            req_id,
            FrontendRequest::ChildTranscript {
                child_sid: houyicoder_protocol::frontend::SessionId::new(
                    SessionId::new().to_string(),
                ),
            },
        )
        .await
        .expect("send dispatch barrier");
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), client.next_frame())
            .await
            .expect("dispatch barrier timeout")
            .expect("dispatch barrier frame");
        if matches!(frame, ServerFrame::Response(response) if response.req_id == req_id) {
            break;
        }
    }
}

/// A queued input on a resumed run is projected before the next model delta.
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "reconnect lifecycle proof")]
async fn test_reconnect_projects_queued_input() {
    let session = SessionId::new();
    let append_notify = Arc::new(Notify::new());
    let store = Arc::new(
        SessionStore::new(Box::new(InMemoryBackend::new()))
            .with_append_notify(append_notify.clone()),
    );
    let entered = Arc::new(Notify::new());
    let (release_tx, release_rx) = oneshot::channel();
    let release = Arc::new(Mutex::new(Some(release_rx)));
    let responses = vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "toolu_approval".into(),
                name: "approvable".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "toolu_blocking".into(),
                name: "reconnect_blocking".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "after queued input".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(responses));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ApprovableTool));
    tools.register(Arc::new(ReconnectBlockingTool {
        entered: entered.clone(),
        release,
    }));

    let (client_tx2, server_rx2) = mpsc::channel::<String>(64);
    let (server_tx2, client_rx2) = mpsc::channel::<String>(64);
    let event_sequencer = EventSequencer::new();
    let mut runner = Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 10,
            ..RunnerConfig::default()
        },
    );
    event_sequencer.install_on(&mut runner);
    let runner = Arc::new(runner);
    let gate: Arc<dyn houyicoder_permission::ModeGate> = Arc::new(DefaultModeGate::new());
    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    host.insert(session, runner, event_sequencer, gate, append_notify);

    let (client_tx1, server_rx1) = mpsc::channel::<String>(64);
    let (server_tx1, client_rx1) = mpsc::channel::<String>(64);
    let io1 = ServerIo::new(server_tx1, server_rx1);
    let host1 = host.clone();
    let serve1 = tokio::spawn(async move {
        drop(serve_session(host1, session, io1).await);
    });
    let mut client1 = Client::new(Box::new(InProcTransport::from_halves(
        client_tx1, client_rx1,
    )));
    client1.connect().await.expect("first handshake");
    client1
        .send_request(
            RequestId(1),
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text { text: "go".into() }],
                disabled_skills: Default::default(),
            },
        )
        .await
        .expect("start interrupted run");
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), client1.next_frame())
            .await
            .expect("first connection frame timeout")
            .expect("first connection frame");
        if matches!(frame, ServerFrame::Request(_)) {
            break;
        }
    }
    drop(client1);
    tokio::time::timeout(Duration::from_secs(2), serve1)
        .await
        .expect("first connection closes")
        .expect("first serve task");

    let io2 = ServerIo::new(server_tx2, server_rx2);
    let host2 = host.clone();
    let serve2 = tokio::spawn(async move {
        drop(serve_session(host2, session, io2).await);
    });
    let mut client2 = Client::new(Box::new(InProcTransport::from_halves(
        client_tx2, client_rx2,
    )));
    client2.connect().await.expect("second handshake");
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), client2.next_frame())
            .await
            .expect("reattach frame timeout")
            .expect("reattach frame");
        if let ServerFrame::Request(ask) = frame {
            let call_id = match ask.payload {
                ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                _ => panic!("expected permission request"),
            };
            client2
                .send_reverse_response(
                    ask.req_id,
                    ClientResponsePayload::Permission(ApprovalDecision {
                        call_id,
                        approved: true,
                        updated_input: None,
                        scope: "once".into(),
                    }),
                )
                .await
                .expect("approve reattached request");
            break;
        }
    }

    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("resumed run entered blocking tool");
    let input = QueuedInput::new("reconnected note");
    client2
        .send_notification(houyicoder_protocol::acp_wire::AcpNotification::new(
            "session/inject",
            serde_json::json!({ "input": input }),
        ))
        .await
        .expect("inject queued input");
    await_inject_dispatch(&mut client2).await;
    release_tx.send(()).expect("release blocking tool");

    let mut frames = Vec::new();
    for _ in 0..256 {
        let frame = tokio::time::timeout(Duration::from_secs(2), client2.next_frame())
            .await
            .expect("ordered reconnect frames timeout")
            .expect("ordered reconnect frame");
        let done = reconnect_text_delta(&frame);
        frames.push(frame);
        if done {
            break;
        }
    }
    let user = frames
        .iter()
        .position(|frame| reconnect_user_message(frame, "reconnected note"))
        .expect("queued input projected into transcript");
    let committed = frames
        .iter()
        .position(|frame| reconnect_commit(frame, &input))
        .expect("queued input committed by stable id");
    let delta = frames
        .iter()
        .position(reconnect_text_delta)
        .expect("subsequent model delta");
    let seqs: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            ServerFrame::Event(event) => Some(event.seq),
            _ => None,
        })
        .collect();
    assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
    assert!(
        user < committed,
        "user input must precede commit: {frames:?}"
    );
    assert!(
        committed < delta,
        "commit must precede subsequent model delta: {frames:?}"
    );

    drop(client2);
    tokio::time::timeout(Duration::from_secs(2), serve2)
        .await
        .expect("second connection closes")
        .expect("second serve task");
}

/// Interruption-after-resume (covers the resume_pending re-interrupt loop):
/// the resumed run itself lands another Interruption, so resume_pending writes
/// a fresh PendingTurn and re-emits the new ask on the same connection. The
/// provider scripts three turns: tool_call (Interruption 1), tool_call again
/// (Interruption 2 after resume), then final text (RunOk after the second
/// resume). The first verdict carries scope=always so the consent-rule arm
/// runs at least once across the reconnect tests.
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
async fn test_reconnect_resumes_interruption() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let tool_call = |id: &str| CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: id.into(),
            name: "approvable".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let final_text = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![
        tool_call("toolu_1"),
        tool_call("toolu_2"),
        final_text,
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ApprovableTool));
    let runner = Arc::new(Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 10,
            ..RunnerConfig::default()
        },
    ));
    let gate: Arc<dyn houyicoder_permission::ModeGate> = Arc::new(DefaultModeGate::new());
    let event_sequencer = EventSequencer::new();
    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    host.insert(
        session,
        runner.clone(),
        event_sequencer,
        gate,
        std::sync::Arc::new(tokio::sync::Notify::new()),
    );

    // Connection 1: drive to the first Interruption, receive ask1, disconnect.
    let (client_tx1, server_rx1) = mpsc::channel::<String>(8);
    let (server_tx1, client_rx1) = mpsc::channel::<String>(8);
    let io1 = ServerIo::new(server_tx1, server_rx1);
    let host1 = host.clone();
    let s1 = session;
    let serve1 = tokio::spawn(async move {
        drop(serve_session(host1, s1, io1).await);
    });
    let mut client1 = Client::new(Box::new(InProcTransport::from_halves(
        client_tx1, client_rx1,
    )));
    client1.connect().await.expect("handshake 1");
    client1
        .send_request(
            RequestId(1),
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "go".to_string(),
                }],
                disabled_skills: Default::default(),
            },
        )
        .await
        .expect("send message 1");
    let mut ask1_call_id = String::new();
    for _ in 0..64 {
        match client1.next_frame().await.expect("server 1 frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(ask) => {
                ask1_call_id = match ask.payload {
                    ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                    _ => panic!("expected a permission ask"),
                };
                break;
            }
            _ => {}
        }
    }
    assert!(!ask1_call_id.is_empty(), "client 1 received ask1");
    drop(client1);
    drop(serve1.await);

    // Connection 2 (reattach): resume_pending re-emits ask1; answer approve
    // with scope=always (exercises the consent-rule arm). The resumed run
    // lands a SECOND Interruption (ask2); resume_pending writes a fresh
    // PendingTurn + re-emits ask2 on the same connection; answer it; the
    // second resume lands final text; the run completes.
    let (client_tx2, server_rx2) = mpsc::channel::<String>(8);
    let (server_tx2, client_rx2) = mpsc::channel::<String>(8);
    let io2 = ServerIo::new(server_tx2, server_rx2);
    let host2 = host.clone();
    let s2 = session;
    let serve2 = tokio::spawn(async move {
        drop(serve_session(host2, s2, io2).await);
    });
    let mut client2 = Client::new(Box::new(InProcTransport::from_halves(
        client_tx2, client_rx2,
    )));
    client2.connect().await.expect("handshake 2");
    let mut answered_call_ids = Vec::new();
    let mut events_received = 0u32;
    for _ in 0..64 {
        match client2.next_frame().await.expect("server 2 frame") {
            ServerFrame::Event(_) => {
                events_received += 1;
            }
            ServerFrame::Request(ask) => {
                let call_id = match ask.payload {
                    ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                    _ => panic!("expected the re-emitted ask"),
                };
                answered_call_ids.push(call_id.clone());
                // First verdict carries scope=always so the consent rule runs;
                // the second is a plain once-scope approve.
                let scope = if answered_call_ids.len() == 1 {
                    "always"
                } else {
                    "once"
                };
                client2
                    .send_reverse_response(
                        ask.req_id,
                        ClientResponsePayload::Permission(ApprovalDecision {
                            call_id,
                            approved: true,
                            updated_input: None,
                            scope: scope.to_string(),
                        }),
                    )
                    .await
                    .expect("answer re-emit");
                // Both asks answered (ask1 re-emit + ask2 from
                // Interruption-after-resume) — break so the resumed run can
                // complete; serve then enters its main loop and stops sending.
                if answered_call_ids.len() == 2 {
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(
        events_received > 0,
        "fresh reattaching client must receive replayed trajectory events (cursor count 0 means full replay)"
    );
    assert_eq!(
        answered_call_ids,
        vec!["toolu_1".to_string(), "toolu_2".to_string()],
        "reattach re-emitted ask1 then the fresh ask2 from Interruption-after-resume",
    );
    await_pending_cleared(&host, session).await;
    drop(client2);
    drop(serve2.await);
    assert!(
        host.store().pending(session).is_none(),
        "parked turn cleared after the double-resume",
    );
}

/// A denied verdict on reconnect (covers the PermissionVerdict::Denied audit
/// arm): the reattaching client answers deny. The runner resumes with the
/// denied decision (the tool does not execute); the second scripted turn is
/// final text, so the run completes. The audit records a Denied verdict.
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
async fn test_deny_verdict_completes_run() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let first = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "toolu_1".into(),
            name: "approvable".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let second = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done after denial".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![first, second]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ApprovableTool));
    let runner = Arc::new(Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    ));
    let gate: Arc<dyn houyicoder_permission::ModeGate> = Arc::new(DefaultModeGate::new());
    let event_sequencer = EventSequencer::new();
    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    host.insert(
        session,
        runner.clone(),
        event_sequencer,
        gate,
        std::sync::Arc::new(tokio::sync::Notify::new()),
    );

    // Connection 1: receive ask1, disconnect before answering.
    let (client_tx1, server_rx1) = mpsc::channel::<String>(8);
    let (server_tx1, client_rx1) = mpsc::channel::<String>(8);
    let io1 = ServerIo::new(server_tx1, server_rx1);
    let host1 = host.clone();
    let s1 = session;
    let serve1 = tokio::spawn(async move {
        drop(serve_session(host1, s1, io1).await);
    });
    let mut client1 = Client::new(Box::new(InProcTransport::from_halves(
        client_tx1, client_rx1,
    )));
    client1.connect().await.expect("handshake 1");
    client1
        .send_request(
            RequestId(1),
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "go".to_string(),
                }],
                disabled_skills: Default::default(),
            },
        )
        .await
        .expect("send message 1");
    let mut ask1_call_id = String::new();
    for _ in 0..64 {
        match client1.next_frame().await.expect("server 1 frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(ask) => {
                ask1_call_id = match ask.payload {
                    ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                    _ => panic!("expected a permission ask"),
                };
                break;
            }
            _ => {}
        }
    }
    assert!(!ask1_call_id.is_empty(), "client 1 received ask1");
    drop(client1);
    drop(serve1.await);

    // Connection 2: re-emit ask1, answer DENY. The runner resumes with the
    // denied decision; the tool is not executed; the second turn is final
    // text; the run completes. The audit trajectory records a Denied verdict.
    let (client_tx2, server_rx2) = mpsc::channel::<String>(8);
    let (server_tx2, client_rx2) = mpsc::channel::<String>(8);
    let io2 = ServerIo::new(server_tx2, server_rx2);
    let host2 = host.clone();
    let s2 = session;
    let serve2 = tokio::spawn(async move {
        drop(serve_session(host2, s2, io2).await);
    });
    let mut client2 = Client::new(Box::new(InProcTransport::from_halves(
        client_tx2, client_rx2,
    )));
    client2.connect().await.expect("handshake 2");
    let mut answered = false;
    for _ in 0..64 {
        match client2.next_frame().await.expect("server 2 frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(ask) => {
                let call_id = match ask.payload {
                    ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                    _ => panic!("expected the re-emitted ask"),
                };
                assert_eq!(call_id, ask1_call_id);
                client2
                    .send_reverse_response(
                        ask.req_id,
                        ClientResponsePayload::Permission(ApprovalDecision {
                            call_id,
                            approved: false,
                            updated_input: None,
                            scope: "once".to_string(),
                        }),
                    )
                    .await
                    .expect("answer deny");
                answered = true;
                break;
            }
            _ => {}
        }
    }
    assert!(answered, "reattach re-emitted ask1 for the deny verdict");
    await_pending_cleared(&host, session).await;
    drop(client2);
    drop(serve2.await);
    // The audit trajectory carries a Denied verdict for the re-answered call.
    let denied = runner
        .store()
        .trajectory_snapshot(session)
        .iter()
        .any(|ev| {
            matches!(
                ev.event,
                houyicoder_context::SessionEvent::PermissionDecision {
                    verdict: houyicoder_context::PermissionVerdict::Denied,
                    ..
                }
            )
        });
    assert!(denied, "denied verdict audited on reconnect-resume");
    assert!(
        host.store().pending(session).is_none(),
        "parked turn cleared after the denied-resume",
    );
}

/// A disconnect before answering a re-emitted permission request preserves
/// the parked turn so another connection can resume it.
#[tokio::test]
async fn test_mid_reemit_disconnect_errors() {
    let (_runner, session, host) = runner_and_host();

    // Connection 1: receive ask1, disconnect before answering.
    let (client_tx1, server_rx1) = mpsc::channel::<String>(8);
    let (server_tx1, client_rx1) = mpsc::channel::<String>(8);
    let io1 = ServerIo::new(server_tx1, server_rx1);
    let host1 = host.clone();
    let s1 = session;
    let serve1 = tokio::spawn(async move {
        drop(serve_session(host1, s1, io1).await);
    });
    let mut client1 = Client::new(Box::new(InProcTransport::from_halves(
        client_tx1, client_rx1,
    )));
    client1.connect().await.expect("handshake 1");
    client1
        .send_request(
            RequestId(1),
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "go".to_string(),
                }],
                disabled_skills: Default::default(),
            },
        )
        .await
        .expect("send message 1");
    for _ in 0..64 {
        match client1.next_frame().await.expect("server 1 frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(_) => break,
            _ => {}
        }
    }
    drop(client1);
    drop(serve1.await);

    // Connection 2: receive the re-emitted ask, then drop without answering.
    let (client_tx2, server_rx2) = mpsc::channel::<String>(8);
    let (server_tx2, client_rx2) = mpsc::channel::<String>(8);
    let io2 = ServerIo::new(server_tx2, server_rx2);
    let host2 = host.clone();
    let s2 = session;
    let serve2 = tokio::spawn(async move {
        drop(serve_session(host2, s2, io2).await);
    });
    let mut client2 = Client::new(Box::new(InProcTransport::from_halves(
        client_tx2, client_rx2,
    )));
    client2.connect().await.expect("handshake 2");
    let mut reemitted = false;
    for _ in 0..64 {
        match client2.next_frame().await.expect("server 2 frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(_) => {
                reemitted = true;
                break;
            }
            _ => {}
        }
    }
    assert!(
        reemitted,
        "reattach re-emitted the ask before the disconnect"
    );
    drop(client2);
    let outcome = tokio::time::timeout(Duration::from_secs(2), serve2).await;
    assert!(
        outcome.is_ok(),
        "serve_session returns (no hang) on mid-re-emit disconnect"
    );
    // The parked turn survives — remaining still holds the ask, decided empty.
    let parked = host
        .store()
        .pending(session)
        .expect("parked turn survives mid-re-emit disconnect");
    assert_eq!(
        parked.remaining.len(),
        1,
        "the unanswered ask still pending"
    );
    assert!(parked.decided.is_empty(), "no verdict advanced");
}

/// A reattaching connection ignores an unrelated reverse response and keeps
/// waiting for the verdict correlated with its permission request.
#[tokio::test]
async fn test_reconnect_drops_wrong_response() {
    let (_runner, session, host) = runner_and_host();

    // Connection 1: receive ask1, disconnect before answering.
    let (client_tx1, server_rx1) = mpsc::channel::<String>(8);
    let (server_tx1, client_rx1) = mpsc::channel::<String>(8);
    let io1 = ServerIo::new(server_tx1, server_rx1);
    let host1 = host.clone();
    let s1 = session;
    let serve1 = tokio::spawn(async move {
        drop(serve_session(host1, s1, io1).await);
    });
    let mut client1 = Client::new(Box::new(InProcTransport::from_halves(
        client_tx1, client_rx1,
    )));
    client1.connect().await.expect("handshake 1");
    client1
        .send_request(
            RequestId(1),
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "go".to_string(),
                }],
                disabled_skills: Default::default(),
            },
        )
        .await
        .expect("send message 1");
    for _ in 0..64 {
        match client1.next_frame().await.expect("server 1 frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(_) => break,
            _ => {}
        }
    }
    drop(client1);
    drop(serve1.await);

    // Connection 2: receive the re-emitted ask, then send a response with a
    // MISMATCHED req_id (not the one the server minted for the re-emit).
    let (client_tx2, server_rx2) = mpsc::channel::<String>(8);
    let (server_tx2, client_rx2) = mpsc::channel::<String>(8);
    let io2 = ServerIo::new(server_tx2, server_rx2);
    let host2 = host.clone();
    let s2 = session;
    let serve2 = tokio::spawn(async move {
        drop(serve_session(host2, s2, io2).await);
    });
    let mut client2 = Client::new(Box::new(InProcTransport::from_halves(
        client_tx2, client_rx2,
    )));
    client2.connect().await.expect("handshake 2");
    let mut reemit_req_id = None;
    for _ in 0..64 {
        match client2.next_frame().await.expect("server 2 frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(ask) => {
                reemit_req_id = Some(ask.req_id);
                break;
            }
            _ => {}
        }
    }
    let reemit_req_id = reemit_req_id.expect("re-emitted ask received");
    // A req_id the server never minted for this re-emit. The ask-wait loop
    // drops it (non-fatal) and keeps waiting for the matching response; the
    // prior fatal arm ended the connection here (the deadlock class).
    let bogus = RequestId(reemit_req_id.0 + 99);
    client2
        .send_reverse_response(
            bogus,
            ClientResponsePayload::Permission(ApprovalDecision {
                call_id: "toolu_1".to_string(),
                approved: true,
                updated_input: None,
                scope: "once".to_string(),
            }),
        )
        .await
        .expect("send mismatched response");
    client2
        .send_reverse_response(
            reemit_req_id,
            ClientResponsePayload::Permission(ApprovalDecision {
                call_id: "toolu_1".to_string(),
                approved: true,
                updated_input: None,
                scope: "once".to_string(),
            }),
        )
        .await
        .expect("send matching response");
    await_inject_dispatch(&mut client2).await;
    assert!(
        !serve2.is_finished(),
        "serve_session must continue after ignoring a mismatched response"
    );
    assert!(
        host.store().pending(session).is_none(),
        "matching verdict advances the parked turn"
    );
    drop(client2);
    drop(serve2.await);
}

/// Terminal session reattach (covers the lease guard's Cancelled/Shutdown
/// arm): a session that was aborted (Cancelled) or handed off (Shutdown)
/// refuses a reattaching serve — there is nothing to re-emit. The serve
/// returns Unavailable without touching the runner or the parked turn.
#[tokio::test]
async fn test_reattach_cancelled_session_errors() {
    let (_runner, session, host) = runner_and_host();
    // Mark the session terminal (the cancel verb does this in the full path;
    // the guard reads the same state field, so a direct flip tests the
    // refuse-reattach contract without coupling to the cancel flow).
    host.store().set_state(session, LifecycleState::Cancelled);
    let (_client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, _client_rx) = mpsc::channel::<String>(8);
    let io = ServerIo::new(server_tx, server_rx);
    let outcome =
        tokio::time::timeout(Duration::from_secs(2), serve_session(host, session, io)).await;
    assert!(
        outcome.is_ok(),
        "serve returns (no hang) on a terminal session"
    );
    let result = outcome.expect("completed");
    assert!(result.is_err(), "terminal session refuses reattach");
    let msg = result.unwrap_err().message;
    assert!(
        msg.contains("terminal"),
        "error names the terminal state: {msg}"
    );
}

/// Occupied lease reattach (covers the lease guard's Running arm): while one
/// connection holds the lease (serve parked at an Interruption, state
/// Running), a second serve on the same session is refused — the
/// single-writer-per-session contract. The second serve returns Unavailable
/// immediately; the first serve is unaffected. After the first disconnects,
/// the lease releases to Detached and a reattach would proceed.
#[tokio::test]
async fn test_reattach_occupied_session_errors() {
    let (_runner, session, host) = runner_and_host();

    // Connection 1: drive to the Interruption so serve1 holds the lease
    // (state Running) and parks at the ask. Do NOT answer — keep it parked.
    let (client_tx1, server_rx1) = mpsc::channel::<String>(8);
    let (server_tx1, client_rx1) = mpsc::channel::<String>(8);
    let io1 = ServerIo::new(server_tx1, server_rx1);
    let host1 = host.clone();
    let s1 = session;
    let serve1 = tokio::spawn(async move {
        drop(serve_session(host1, s1, io1).await);
    });
    let mut client1 = Client::new(Box::new(InProcTransport::from_halves(
        client_tx1, client_rx1,
    )));
    client1.connect().await.expect("handshake 1");
    client1
        .send_request(
            RequestId(1),
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "go".to_string(),
                }],
                disabled_skills: Default::default(),
            },
        )
        .await
        .expect("send message 1");
    for _ in 0..64 {
        match client1.next_frame().await.expect("server 1 frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(_) => break,
            _ => {}
        }
    }
    // serve1 is now parked at the ask; the lease is Running.
    assert_eq!(
        host.store().state(session),
        LifecycleState::Running,
        "serve1 holds the lease"
    );

    // Connection 2 (concurrent reattach): the guard sees Running and refuses
    // before any frame is exchanged, so no client is built for it.
    let (_client_tx2, server_rx2) = mpsc::channel::<String>(8);
    let (server_tx2, _client_rx2) = mpsc::channel::<String>(8);
    let io2 = ServerIo::new(server_tx2, server_rx2);
    let host2 = host.clone();
    let s2 = session;
    let outcome = tokio::time::timeout(Duration::from_secs(2), serve_session(host2, s2, io2)).await;
    assert!(
        outcome.is_ok(),
        "second serve returns (no hang) on occupied lease"
    );
    let result = outcome.expect("completed");
    assert!(
        result.is_err(),
        "occupied lease refuses a concurrent reattach"
    );
    let msg = result.unwrap_err().message;
    assert!(
        msg.contains("lease held"),
        "error names the held lease: {msg}"
    );

    // Release the first connection: serve1 returns, the lease drops to
    // Detached, and a fresh reattach would proceed again (state checks out).
    drop(client1);
    drop(serve1.await);
    assert_eq!(
        host.store().state(session),
        LifecycleState::Detached,
        "lease released after the holder disconnects"
    );
}
