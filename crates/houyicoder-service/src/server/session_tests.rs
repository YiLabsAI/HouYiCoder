use super::*;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::provider::stream_from_response;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::{PFut, PStream};
use houyicoder_client::{Client, InProcTransport};
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::{
    ClientResponsePayload, RequestId, ServerFrame, ServerRequestPayload,
};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalRequest, ContentBlock};
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, ModelCapabilities, OutputItem, ProviderError, Usage,
};
use houyicoder_session::SessionStore;
use serde_json::Value;

use crate::composition::SessionHost;
use crate::lifecycle::SessionLeaseStore;

/// A scripted provider: returns a queued response per complete/stream call.
pub struct FakeProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
}
impl FakeProvider {
    pub fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }
}
impl ModelProvider for FakeProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let r = self
            .responses
            .lock()
            .expect("script mutex")
            .pop_front()
            .expect("scripted response");
        Box::pin(async move { Ok(r) })
    }
    fn stream(
        &self,
        _req: CompletionRequest,
    ) -> PStream<'_, Result<houyicoder_protocol::llm::LlmEvent, ProviderError>> {
        let r = self
            .responses
            .lock()
            .expect("script mutex")
            .pop_front()
            .expect("scripted response");
        stream_from_response(r)
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A tool that requires approval, returning a fixed result when approved.
pub struct ApprovableTool;
impl Tool for ApprovableTool {
    fn name(&self) -> &str {
        "approvable"
    }
    fn description(&self) -> &str {
        "a tool that needs approval"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        _input: Value,
    ) -> PFut<'_, Result<Value, houyicoder_protocol::extension::ToolError>> {
        Box::pin(async move { Ok(serde_json::json!({"ok": true})) })
    }
    fn requires_approval(&self) -> bool {
        true
    }
}

/// Build a runner whose first scripted response is a tool call to the
/// approvable tool (so the run suspends at Interruption), and whose second
/// response is final text (so the resumed run ends FinalOutput). Paired
/// with a SessionHost so the serve_session path re-hydrates from it.
pub fn runner_and_host() -> (Arc<Runner>, SessionId, Arc<SessionHost>) {
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
            text: "done after approval".into(),
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
    let next_seq = Arc::new(AtomicU64::new(0));
    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    host.insert(session, runner.clone(), next_seq, gate);
    (runner, session, host)
}

/// One connection through serve_session: the run lands an Interruption, the
/// client answers approve, the run resumes to FinalOutput. The parked
/// PendingTurn is written on Interruption, advanced per verdict, and cleared
/// before resume — so after the run the host store has no parked turn.
#[tokio::test]
async fn test_serve_drives_parked_turn() {
    let (_runner, session, host) = runner_and_host();
    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    let server_io = ServerIo::new(server_tx, server_rx);
    let client_transport = InProcTransport::from_halves(client_tx, client_rx);
    let mut client = Client::new(Box::new(client_transport));

    let host_clone = host.clone();
    let serve_session_session = session;
    tokio::spawn(async move {
        drop(serve_session(host_clone, serve_session_session, server_io).await);
    });

    client.connect().await.expect("handshake");
    let req_id = RequestId(1);
    client
        .send_request(
            req_id,
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "go".to_string(),
                }],
            },
        )
        .await
        .expect("send message");

    // Drain turn events until the reverse Permission ask arrives; answer it
    // approve; the resumed run lands RunOk.
    let mut got_run_ok = false;
    for _ in 0..64 {
        match client.next_frame().await.expect("server frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(ask) => {
                let ask_call_id = match ask.payload {
                    ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                    _ => panic!("expected a permission reverse request"),
                };
                // While parked at Interruption (before the verdict), the
                // host store holds the turn with the one unanswered ask.
                let parked = host.store().pending(session).expect("parked turn");
                assert_eq!(parked.remaining.len(), 1, "one unanswered ask");
                assert_eq!(parked.remaining[0].call_id, ask_call_id);
                assert!(parked.decided.is_empty(), "no verdicts yet");
                client
                    .send_reverse_response(
                        ask.req_id,
                        ClientResponsePayload::Permission(ApprovalDecision {
                            call_id: ask_call_id,
                            approved: true,
                            updated_input: None,
                            scope: "once".to_string(),
                        }),
                    )
                    .await
                    .expect("answer reverse request");
            }
            ServerFrame::Response(resp) if resp.req_id == req_id => {
                got_run_ok = true;
                break;
            }
            _ => {}
        }
    }
    assert!(got_run_ok, "client received the RunOk outcome");
    // After resume the parked turn is cleared — a reconnect mid-resume
    // would not re-emit a turn that already completed.
    assert!(
        host.store().pending(session).is_none(),
        "parked turn cleared on resume"
    );
}

use std::time::Duration;

/// Poll the host store until the parked turn is cleared (resume completed).
/// Deterministic — does not depend on frame-drain timing, which coverage
/// instrumentation skews. The post-resume events fit in the server->client
/// channel buffer (cap 8), so resume_pending completes without the client
/// reading.
pub async fn await_pending_cleared(host: &SessionHost, session: SessionId) {
    for _ in 0..400 {
        if host.store().pending(session).is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("parked turn not cleared within 4s — resume did not complete");
}

/// Reconnect-replay: a client disconnects mid-permission-ask. The session
/// survives (runner + parked PendingTurn retained). A reattaching connection's
/// serve re-emits the pending ask; the reattaching client answers; the run
/// resumes against the same Arc<Runner> + lands FinalOutput. The parked turn
/// is cleared on resume.
#[tokio::test]
async fn test_reconnect_replays_pending_ask() {
    let (_runner, session, host) = runner_and_host();

    // Connection 1: drive to the Interruption, receive the ask, then
    // disconnect (drop the client before answering).
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
            },
        )
        .await
        .expect("send message 1");
    let mut ask_call_id = String::new();
    for _ in 0..64 {
        match client1.next_frame().await.expect("server 1 frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(ask) => {
                ask_call_id = match ask.payload {
                    ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                    _ => panic!("expected a permission ask"),
                };
                break;
            }
            _ => {}
        }
    }
    assert!(!ask_call_id.is_empty(), "client 1 received the ask");
    let parked = host
        .store()
        .pending(session)
        .expect("parked turn on disc 1");
    assert_eq!(parked.remaining.len(), 1);
    assert_eq!(parked.remaining[0].call_id, ask_call_id);
    assert!(parked.decided.is_empty());
    drop(client1);
    drop(serve1.await);

    // Connection 2 (reattach, same host + session): the parked turn is
    // re-emitted after the handshake, before the client sends anything.
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
            ServerFrame::Request(ask) => {
                let call_id = match ask.payload {
                    ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                    _ => panic!("expected the re-emitted permission ask"),
                };
                assert_eq!(call_id, ask_call_id, "re-emit carries the original call_id");
                reemitted = true;
                client2
                    .send_reverse_response(
                        ask.req_id,
                        ClientResponsePayload::Permission(ApprovalDecision {
                            call_id,
                            approved: true,
                            updated_input: None,
                            scope: "once".to_string(),
                        }),
                    )
                    .await
                    .expect("answer re-emit");
                break;
            }
            _ => {}
        }
    }
    assert!(reemitted, "reattach re-emitted the pending ask");

    // The run resumed + completed — poll the host until the parked turn is
    // cleared (deterministic; no frame-drain timing dependency).
    await_pending_cleared(&host, session).await;
    drop(client2);
    drop(serve2.await);
    assert!(
        host.store().pending(session).is_none(),
        "parked turn cleared after reconnect-resume"
    );
}

/// Multi-approval mid-batch disconnect (the PendingTurn correctness
/// regression): Interruption with [a1, a2, a3]. Client 1 answers a1 (moves
/// to decided), is asked a2, then disconnects before a2's verdict. The
/// reattach re-emits a2 + a3 (NOT a1 — already decided), the reattaching
/// client answers both, and runner.resume receives all three decisions.
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
async fn test_reconnect_batch_preserves_decided() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let first = CompletionResponse {
        output: (1..=3)
            .map(|i| OutputItem::ToolCall {
                id: format!("toolu_{i}"),
                name: "approvable".into(),
                input: serde_json::json!({}),
            })
            .collect::<Vec<_>>(),
        usage: Usage::default(),
        model: "test".into(),
    };
    let second = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
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
            max_turns: 10,
            ..RunnerConfig::default()
        },
    ));
    let gate: Arc<dyn houyicoder_permission::ModeGate> = Arc::new(DefaultModeGate::new());
    let next_seq = Arc::new(AtomicU64::new(0));
    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    host.insert(session, runner.clone(), next_seq, gate);

    // Connection 1: answer a1, then disconnect when a2 arrives.
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
            },
        )
        .await
        .expect("send message 1");
    let mut answered = 0;
    for _ in 0..64 {
        match client1.next_frame().await.expect("server 1 frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(ask) => {
                let call_id = match ask.payload {
                    ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                    _ => panic!("expected a permission ask"),
                };
                if answered == 0 {
                    client1
                        .send_reverse_response(
                            ask.req_id,
                            ClientResponsePayload::Permission(ApprovalDecision {
                                call_id,
                                approved: true,
                                updated_input: None,
                                scope: "once".to_string(),
                            }),
                        )
                        .await
                        .expect("answer a1");
                    answered += 1;
                } else {
                    break;
                }
            }
            _ => {}
        }
    }
    assert_eq!(answered, 1, "answered a1, then disconnected at a2");
    drop(client1);
    drop(serve1.await);

    let parked = host
        .store()
        .pending(session)
        .expect("parked mid-batch turn");
    assert_eq!(parked.remaining.len(), 2, "a2 + a3 still unanswered");
    assert_eq!(parked.decided.len(), 1, "a1 verdict preserved");
    assert_eq!(parked.decided[0].call_id, "toolu_1");

    // Reattach: resume_pending re-emits a2 + a3 (not a1).
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
    let mut reemitted_call_ids = Vec::new();
    for _ in 0..64 {
        match client2.next_frame().await.expect("server 2 frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(ask) => {
                let call_id = match ask.payload {
                    ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                    _ => panic!("expected the re-emitted ask"),
                };
                reemitted_call_ids.push(call_id.clone());
                client2
                    .send_reverse_response(
                        ask.req_id,
                        ClientResponsePayload::Permission(ApprovalDecision {
                            call_id,
                            approved: true,
                            updated_input: None,
                            scope: "once".to_string(),
                        }),
                    )
                    .await
                    .expect("answer re-emit");
                if reemitted_call_ids.len() == 2 {
                    break;
                }
            }
            _ => {}
        }
    }
    assert_eq!(
        reemitted_call_ids,
        vec!["toolu_2".to_string(), "toolu_3".to_string()],
        "re-emit skips a1 (already decided), re-sends a2 + a3",
    );
    await_pending_cleared(&host, session).await;
    drop(client2);
    drop(serve2.await);
    assert!(
        host.store().pending(session).is_none(),
        "parked turn cleared after the mid-batch reconnect-resume",
    );
}

/// A tool that needs no approval: returns a fixed result so a tool-call turn
/// resolves to RunAgain (not Interruption). Lets the mid-turn-injection tests
/// drive a multi-turn run without the permission popup.
pub struct NoopTool;
impl Tool for NoopTool {
    fn name(&self) -> &str {
        "noop"
    }
    fn description(&self) -> &str {
        "returns a fixed result, no approval"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        _input: Value,
    ) -> PFut<'_, Result<Value, houyicoder_protocol::extension::ToolError>> {
        Box::pin(async move { Ok(serde_json::json!({"ok": true})) })
    }
}

/// Build a runner whose first response is a tool call to NoopTool (RunAgain)
/// + second is final text. For the mid-turn-injection integration tests.
fn runner_with_noop() -> (Arc<Runner>, SessionId, Arc<SessionHost>) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let first = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "toolu_1".into(),
            name: "noop".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let second = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![first, second]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(NoopTool));
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
    let next_seq = Arc::new(AtomicU64::new(0));
    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    host.insert(session, runner.clone(), next_seq, gate);
    (runner, session, host)
}

/// A session/inject notification that lands between runs (no active run) is
/// enqueued for the next run; the next run's drive loop drains it at the
/// RunAgain boundary + the server reports the consumed text back as a
/// QueueConsumed event so the frontend can drop it from its mirror.
#[tokio::test]
async fn test_between_runs_inject_drains() {
    let (_runner, session, host) = runner_with_noop();
    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    let server_io = ServerIo::new(server_tx, server_rx);
    let client_transport = InProcTransport::from_halves(client_tx, client_rx);
    let mut client = Client::new(Box::new(client_transport));

    let host_clone = host.clone();
    let sess = session;
    tokio::spawn(async move {
        drop(serve_session(host_clone, sess, server_io).await);
    });
    client.connect().await.expect("handshake");

    // Inject while no run is active: the between-runs loop catches the
    // session/* notification + enqueues it for the next run.
    client
        .send_notification(houyicoder_protocol::acp_wire::AcpNotification::new(
            "session/inject",
            serde_json::json!({ "sessionId": session.to_string(), "text": "extra note" }),
        ))
        .await
        .expect("send inject");

    let req_id = RequestId(1);
    client
        .send_request(
            req_id,
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "go".to_string(),
                }],
            },
        )
        .await
        .expect("send message");

    let mut got_run_ok = false;
    let mut got_consumed = false;
    for _ in 0..64 {
        match client.next_frame().await.expect("server frame") {
            ServerFrame::Event(ev) => {
                if matches!(
                    ev.payload,
                    houyicoder_protocol::frontend::FrontendEventKind::QueueConsumed {
                        ref texts
                    } if texts == &vec!["extra note".to_string()]
                ) {
                    got_consumed = true;
                }
            }
            ServerFrame::Response(resp) if resp.req_id == req_id => {
                got_run_ok = true;
                break;
            }
            _ => {}
        }
    }
    assert!(got_run_ok, "run completed");
    assert!(
        got_consumed,
        "QueueConsumed event reported the injected text",
    );
}

/// A session/inject notification sent WHILE the run is mid-flight is caught
/// by the mid-run select + enqueued; the drive loop drains it at the next
/// RunAgain boundary + the server reports the consumed text via QueueConsumed.
/// The NoopTool sleeps briefly so the run stays mid-flight across the tool
/// call, giving the inject notification a window to arrive during the select.
pub struct SleepNoopTool;
impl Tool for SleepNoopTool {
    fn name(&self) -> &str {
        "noop"
    }
    fn description(&self) -> &str {
        "returns a fixed result after a short sleep"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    fn execute(
        &self,
        _ctx: ToolCtx,
        _input: Value,
    ) -> PFut<'_, Result<Value, houyicoder_protocol::extension::ToolError>> {
        Box::pin(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok(serde_json::json!({"ok": true}))
        })
    }
}

/// A session/inject sent mid-run is caught by the mid-run select arm +
/// drained at the RunAgain boundary; the server reports the consumed text
/// back as a QueueConsumed event.
#[tokio::test]
async fn test_during_run_inject_caught() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let first = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "toolu_1".into(),
            name: "noop".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let second = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![first, second]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SleepNoopTool));
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
    let next_seq = Arc::new(AtomicU64::new(0));
    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    host.insert(session, runner.clone(), next_seq, gate);

    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    let server_io = ServerIo::new(server_tx, server_rx);
    let client_transport = InProcTransport::from_halves(client_tx, client_rx);
    let mut client = Client::new(Box::new(client_transport));

    let host_clone = host.clone();
    let sess = session;
    tokio::spawn(async move {
        drop(serve_session(host_clone, sess, server_io).await);
    });
    client.connect().await.expect("handshake");

    let req_id = RequestId(1);
    client
        .send_request(
            req_id,
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "go".to_string(),
                }],
            },
        )
        .await
        .expect("send message");

    // The run is now mid-flight (turn 1 is the noop tool call). Send the
    // inject so the mid-run select catches it before the run advances.
    client
        .send_notification(houyicoder_protocol::acp_wire::AcpNotification::new(
            "session/inject",
            serde_json::json!({ "sessionId": session.to_string(), "text": "mid note" }),
        ))
        .await
        .expect("send inject");

    let mut got_run_ok = false;
    let mut got_consumed = false;
    for _ in 0..64 {
        match client.next_frame().await.expect("server frame") {
            ServerFrame::Event(ev) => {
                if matches!(
                    ev.payload,
                    houyicoder_protocol::frontend::FrontendEventKind::QueueConsumed {
                        ref texts
                    } if texts == &vec!["mid note".to_string()]
                ) {
                    got_consumed = true;
                }
            }
            ServerFrame::Response(resp) if resp.req_id == req_id => {
                got_run_ok = true;
                break;
            }
            _ => {}
        }
    }
    assert!(got_run_ok, "run completed");
    assert!(
        got_consumed,
        "QueueConsumed event reported the mid-run inject"
    );
}

#[cfg(test)]
#[path = "session_queue_tests.rs"]
mod queue_lifecycle;
