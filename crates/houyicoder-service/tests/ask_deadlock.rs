//! Ask-wait + approval-flow tests: the reverse-permission round-trip, the
//! ask-wait non-fatal-on-non-matching-frame fix, mid-ask cancel, and idle
//! cancel. Split from client_server_contract.rs so that file stays under the
//! size gate.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use futures::SinkExt;
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
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::envelope::{
    ClientResponsePayload, RequestId, ResponsePayload, ServerFrame, ServerRequestPayload,
};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::run::{
    ApprovalDecision, ApprovalRequest, ContentBlock, RunOutcome, StopReason,
};
use houyicoder_protocol::llm::{
    CompletionResponse, ModelCapabilities, OutputItem, ProviderError, Usage,
};
use houyicoder_service::server::{Server, ServerIo};
use houyicoder_session::SessionStore;
use serde_json::Value;

/// A scripted provider that returns a queued sequence of responses, one per
/// complete() call. Used to drive a multi-turn run: first a tool call (so the
/// runner surfaces an interruption), then a final text reply (so the resumed
/// run ends with FinalOutput).
struct FakeProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
}

impl FakeProvider {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }
}

impl ModelProvider for FakeProvider {
    fn complete(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let r = self
            .responses
            .lock()
            .expect("script mutex")
            .pop_front()
            .expect("scripted response available");
        Box::pin(async move { Ok(r) })
    }
    fn stream(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> PStream<'_, Result<houyicoder_protocol::llm::LlmEvent, ProviderError>> {
        let r = self
            .responses
            .lock()
            .expect("script mutex")
            .pop_front()
            .expect("scripted response available");
        stream_from_response(r)
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A tool that requires approval and, when approved, returns a fixed result.
/// Used so the runner surfaces an interruption the service projects as a
/// reverse permission request.
struct ApprovableTool;

impl Tool for ApprovableTool {
    fn name(&self) -> &str {
        "approvable"
    }
    fn description(&self) -> &str {
        "a tool that needs approval"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
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

/// The reverse-permission end-to-end: a MessageSend drives a run whose first
/// provider response is a tool call to a tool that requires approval. The
/// runner suspends (Interruption); the service projects that as a server-to-
/// client reverse Permission request; the client answers approve; the service
/// resumes; the second provider response is final text; the run ends FinalOutput
/// with stop_reason EndTurn. This proves the half-live turn state machine.
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
async fn test_reverse_request_permission_flow() {
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
    let runner = Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );

    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    let server_io = ServerIo::new(server_tx, server_rx);
    let client_transport = InProcTransport::from_halves(client_tx, client_rx);
    let mut client = Client::new(Box::new(client_transport));
    let server = Server::new(
        Arc::new(runner),
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

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

    // Drain: the service forwards turn events, then a reverse Permission
    // request, then (after the client answers) the RunOk response.
    let mut got_final = false;
    for _ in 0..64 {
        match client.next_frame().await.expect("server frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(ask) => {
                let ask_call_id = match ask.payload {
                    ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                    _ => panic!("expected a permission reverse request"),
                };
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
            ServerFrame::Response(resp) if resp.req_id == req_id => match resp.payload {
                ResponsePayload::RunOk(run) => {
                    assert_eq!(run.stop_reason, StopReason::EndTurn);
                    match run.outcome {
                        RunOutcome::FinalOutput { content } => {
                            let text = content
                                .into_iter()
                                .find_map(|b| match b {
                                    ContentBlock::Text { text } => Some(text),
                                    _ => None,
                                })
                                .expect("a text block");
                            assert_eq!(text, "done after approval");
                        }
                        other => panic!("expected FinalOutput, got {other:?}"),
                    }
                    got_final = true;
                    break;
                }
                other => panic!("expected RunOk, got {other:?}"),
            },
            _ => {}
        }
    }
    assert!(got_final, "the resumed run produced a final outcome");
    drop(client);
    drop(handle.await);
}

/// The ask-wait window must not fatal on a frame that is not the matching
/// permission response. A reconnecting client's first status tick, or a
/// racing poll, sends a Status Request while a reverse permission ask is in
/// flight; the ask-wait read loop must survive that frame (dispatch/drop it
/// and keep waiting) and still pair the real permission response that follows.
/// Pins the root cause of the AskUserQuestion deadlock: the only read point
/// that deviates from the mid-run and resume select paradigm (which treats
/// non-matching frames as non-fatal) is the ask-wait fatal arm on a
/// non-response frame.
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
async fn test_mid_ask_survives_status() {
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
    let runner = Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );

    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    let server_io = ServerIo::new(server_tx, server_rx);
    let client_transport = InProcTransport::from_halves(client_tx, client_rx);
    let mut client = Client::new(Box::new(client_transport));
    let server = Server::new(
        Arc::new(runner),
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

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

    let mut got_final = false;
    for _ in 0..64 {
        let frame = match client.next_frame().await {
            Ok(f) => f,
            Err(_) => break,
        };
        match frame {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(ask) => {
                let ask_call_id = match ask.payload {
                    ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                    _ => panic!("expected a permission reverse request"),
                };
                // Inject a non-matching Status Request mid-ask BEFORE
                // answering. A fatal ask-wait returns InvalidFrame and closes
                // the connection here; the run never resumes.
                client
                    .send_request(RequestId(2), FrontendRequest::Status)
                    .await
                    .expect("send status");
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
            ServerFrame::Response(resp) if resp.req_id == req_id => match resp.payload {
                ResponsePayload::RunOk(run) => {
                    assert_eq!(run.stop_reason, StopReason::EndTurn);
                    match run.outcome {
                        RunOutcome::FinalOutput { content } => {
                            let text = content
                                .into_iter()
                                .find_map(|b| match b {
                                    ContentBlock::Text { text } => Some(text),
                                    _ => None,
                                })
                                .expect("a text block");
                            assert_eq!(text, "done after approval");
                        }
                        other => panic!("expected FinalOutput, got {other:?}"),
                    }
                    got_final = true;
                    break;
                }
                other => panic!("expected RunOk, got {other:?}"),
            },
            _ => {}
        }
    }
    assert!(
        got_final,
        "the ask-wait fatalled on the mid-ask Status frame; the run never resumed"
    );
    drop(client);
    drop(handle.await);
}

/// Esc mid-ask sends session/cancel. The ask-wait must abort the run and
/// return (a deny) so the serve loop resumes the cancelled run instead of
/// hanging on a response the client will not send. The run must end (a
/// Response for the MessageSend req_id) within a timeout — a re-ask loop or
/// a hang fails here.
#[tokio::test]
async fn test_mid_ask_cancel_exits() {
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
    let runner = Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );

    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    // Keep a raw sender for the session/cancel notification (a notification
    // has no req_id, so it is not a ClientFrame::Request the Client tracks).
    let mut raw_tx = client_tx.clone();
    let server_io = ServerIo::new(server_tx, server_rx);
    let client_transport = InProcTransport::from_halves(client_tx, client_rx);
    let mut client = Client::new(Box::new(client_transport));
    let server = Server::new(
        Arc::new(runner),
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

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

    // Drain until the Permission ask arrives, then send session/cancel.
    let mut asked = false;
    for _ in 0..64 {
        match client.next_frame().await.expect("server frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(_) => {
                let cancel = AcpNotification::new("session/cancel", serde_json::json!({}));
                let mut f = houyicoder_protocol::framing::encode(&cancel).expect("encode");
                if !f.ends_with('\n') {
                    f.push('\n');
                }
                raw_tx.send(f).await.expect("send cancel");
                asked = true;
                break;
            }
            _ => {}
        }
    }
    assert!(asked, "the permission ask arrived before cancel");

    // The cancel must end the run (a Response for req_id 1) within 2s. A
    // re-ask loop (cancel -> Interrupted -> re-ask) or a hang fails here.
    let mut ended = false;
    for _ in 0..64 {
        match tokio::time::timeout(Duration::from_secs(2), client.next_frame()).await {
            Ok(Ok(ServerFrame::Response(resp))) if resp.req_id == req_id => {
                ended = true;
                break;
            }
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    assert!(
        ended,
        "cancel mid-ask must end the run, not deadlock or re-ask"
    );
    // Drop both senders so the server's rx hits EOF and serve_session ends;
    // raw_tx (the clone for the notification) would otherwise keep the
    // channel open and handle.await would block.
    drop(raw_tx);
    drop(client);
    drop(handle.await);
}

/// A multi-approval batch: the model emits two approval-requiring tool calls
/// in one turn. The user cancels mid-ask #1 (session/cancel, not a deny
/// response). The serve loop must break the approval for-loop (do not ask #2),
/// and resume() must surface the durable abort as Interrupted so the run ends
/// — no second Permission ask, no hang. This is the case the prior
/// mid_ask_cancel_exits did not cover (a single-approval batch + a scripted
/// final response masked the cancel mechanism).
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
async fn test_mid_cancel_multi_approval() {
    use houyicoder_protocol::acp_wire::AcpNotification;
    use houyicoder_protocol::envelope::{ServerFrame, ServerRequestPayload};
    use houyicoder_protocol::frontend::run::ApprovalRequest;

    let store: Arc<dyn houyicoder_api::session::SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let log = store.clone();
    let session = SessionId::new();
    // Two approval-requiring calls in one turn → Interruption([a1, a2]).
    let first = CompletionResponse {
        output: vec![
            OutputItem::ToolCall {
                id: "toolu_1".into(),
                name: "approvable".into(),
                input: serde_json::json!({}),
            },
            OutputItem::ToolCall {
                id: "toolu_2".into(),
                name: "approvable".into(),
                input: serde_json::json!({}),
            },
        ],
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
    let runner = Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );

    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    let mut raw_tx = client_tx.clone();
    let server_io = ServerIo::new(server_tx, server_rx);
    let client_transport = InProcTransport::from_halves(client_tx, client_rx);
    let mut client = Client::new(Box::new(client_transport));
    let server = Server::new(
        Arc::new(runner),
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

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

    // Drain until the FIRST Permission ask arrives, then send session/cancel.
    let mut asked = false;
    for _ in 0..64 {
        match client.next_frame().await.expect("server frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(_) => {
                let cancel = AcpNotification::new("session/cancel", serde_json::json!({}));
                let mut f = houyicoder_protocol::framing::encode(&cancel).expect("encode");
                if !f.ends_with('\n') {
                    f.push('\n');
                }
                raw_tx.send(f).await.expect("send cancel");
                asked = true;
                break;
            }
            _ => {}
        }
    }
    assert!(asked, "the first permission ask arrived");

    // After cancel: NO second Permission ask, and the run ends (RunOk for the
    // MessageSend) within 2s. A second ask or a hang fails here.
    let mut second_ask = false;
    let mut ended = false;
    for _ in 0..64 {
        match tokio::time::timeout(Duration::from_secs(2), client.next_frame()).await {
            Ok(Ok(ServerFrame::Request(req))) => {
                if matches!(
                    req.payload,
                    ServerRequestPayload::Permission(ApprovalRequest { .. })
                ) {
                    second_ask = true;
                }
            }
            Ok(Ok(ServerFrame::Response(resp))) if resp.req_id == req_id => {
                ended = true;
                break;
            }
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    assert!(!second_ask, "cancel mid-ask #1 must not raise ask #2");
    assert!(
        ended,
        "cancel mid-ask must end the run (RunOk via the durable abort), not hang"
    );
    // Log-consistency: cancel must reconcile orphan tool results so every
    // ToolCall has a matching ToolResult. Without it the next message would
    // ship an assistant turn with tool_calls but no results — providers 400
    // and the durable session is bricked. This is the assertion that pins the
    // blocker fix (the short-circuit must call reconcile_tool_results).
    let events = log.replay(session).await.expect("replay");
    let mut answered = std::collections::HashSet::new();
    for e in &events {
        if let houyicoder_context::TurnEventKind::ToolResult { call_id, .. } = &e.kind {
            answered.insert(call_id.clone());
        }
    }
    let orphans: Vec<String> = events
        .iter()
        .filter_map(|e| match &e.kind {
            houyicoder_context::TurnEventKind::ToolCall { call_id, .. }
                if !answered.contains(call_id) =>
            {
                Some(call_id.clone())
            }
            _ => None,
        })
        .collect();
    assert!(
        orphans.is_empty(),
        "cancel left orphan ToolCalls without ToolResults (session bricked): {orphans:?}"
    );
    drop(raw_tx);
    drop(client);
    drop(handle.await);
}

/// An idle RunCancel request (no active run) must NOT set the durable aborted
/// flag. dispatch.rs routes RunCancel to abort(); the durable flag set by an
/// idle abort survives across the Interruption boundary (no run() between the
/// cancel and a later reconnect-resume clears it) and silently short-circuits
/// that resume — dropping a later approval with no signal. The paused flag
/// makes abort() set the durable flag only when the run is paused on an ask,
/// so an idle/in-flight cancel cannot poison a later resume. This pins it:
/// is_aborted stays false after an idle RunCancel. Red when abort() sets the
/// flag unconditionally (the four-door whack-a-mole); green with the paused
/// gate.
#[tokio::test]
async fn test_idle_cancel_skips_abort() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![]));
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
    let runner_for_check = runner.clone();
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

    // Idle RunCancel request (no run active) → dispatch.rs → abort(). The
    // paused gate means abort does NOT set the durable flag (not paused).
    client
        .send_request(
            RequestId(2),
            FrontendRequest::RunCancel {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                reason: "test".into(),
            },
        )
        .await
        .expect("send run-cancel");
    // Drain the Ack + give the server time to process.
    for _ in 0..16 {
        match tokio::time::timeout(Duration::from_millis(200), client.next_frame()).await {
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    assert!(
        !runner_for_check.is_aborted(),
        "an idle RunCancel must not set the durable aborted flag (it would poison a later resume)"
    );
    drop(client);
    drop(handle.await);
}
