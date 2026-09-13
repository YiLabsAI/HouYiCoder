//! Connection-boundary failure tests: a garbage frame, a bare reverse
//! response between runs, a client that closes before the handshake
//! completes, and a client that closes while a resumed run is active. Each
//! must fail closed with a typed protocol error and never take the
//! connection down silently.

#![cfg(test)]

use super::Server;
use super::frame_carrier::FrameCarrier;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponseEnvelope, ClientResponsePayload, RequestEnvelope, RequestId,
    ResponsePayload, ServerFrame, ServerRequestPayload,
};
use houyicoder_protocol::error::ErrorCategory;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::run::ApprovalDecision;
use houyicoder_protocol::frontend::trust::TrustAccept;
use houyicoder_protocol::handshake::Hello;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use houyicoder_session::SessionStore;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, oneshot};

fn stub_runner() -> Arc<Runner> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Arc::new(Runner::with_shared_store(
        store,
        Arc::new(houyicoder_provider::FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    ))
}

fn stub_server() -> Server {
    Server::new(
        stub_runner(),
        SessionId::new(),
        Arc::new(DefaultModeGate::new()),
    )
}

fn send_line(tx: &mut mpsc::Sender<String>, frame: &impl serde::Serialize) {
    let mut s = houyicoder_protocol::framing::encode(frame).unwrap();
    if !s.ends_with('\n') {
        s.push('\n');
    }
    tx.try_send(s).unwrap();
}

async fn recv_frame(rx: &mut mpsc::Receiver<String>) -> ServerFrame {
    serde_json::from_str(&rx.next().await.unwrap()).expect("frame decodes")
}

async fn recv_error(rx: &mut mpsc::Receiver<String>) -> houyicoder_protocol::error::ProtocolError {
    match recv_frame(rx).await {
        ServerFrame::Response(r) => match r.payload {
            ResponsePayload::Error(e) => e,
            other => panic!("expected Error, got {other:?}"),
        },
        other => panic!("expected response, got {other:?}"),
    }
}

/// A frame that parses as nothing is answered with an InvalidFrame error and
/// the loop keeps serving: one bad frame must not end the session.
#[tokio::test]
async fn test_garbage_frame_rejected() {
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let handle = tokio::spawn(async move { stub_server().serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);

    client_tx.try_send("not json at all".into()).unwrap();
    let err = recv_error(&mut client_rx).await;
    assert_eq!(err.category, ErrorCategory::InvalidFrame);

    // The loop is still alive: a valid request still gets its reply.
    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(1),
            FrontendRequest::PermissionRules,
        )),
    );
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => assert!(
            matches!(r.payload, ResponsePayload::PermissionRules(_)),
            "expected the rule set after the garbage frame, got {:?}",
            r.payload
        ),
        other => panic!("expected response, got {other:?}"),
    }
    handle.abort();
}

/// A reverse response arriving between runs pairs with no open ask. It is
/// answered with an InvalidFrame error naming the orphan req_id rather than
/// dropped silently.
#[tokio::test]
async fn test_bare_reverse_response_rejected() {
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let handle = tokio::spawn(async move { stub_server().serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);

    send_line(
        &mut client_tx,
        &ClientFrame::Response(ClientResponseEnvelope::new(
            RequestId(9),
            ClientResponsePayload::TrustAccept(TrustAccept { accepted: true }),
        )),
    );
    let err = recv_error(&mut client_rx).await;
    assert_eq!(err.category, ErrorCategory::InvalidFrame);
    assert_eq!(err.message, "unexpected reverse response for req_id 9");
    handle.abort();
}

/// A client that closes before sending its Hello ends the handshake with an
/// Unavailable error: the session never opens, and the failure is typed.
#[tokio::test]
async fn test_client_closed_before_hello() {
    let (server_tx, mut client_rx) = mpsc::channel::<String>(16);
    let (client_tx, server_rx) = mpsc::channel::<String>(16);
    let io = FrameCarrier::new(server_tx, server_rx);
    let handle = tokio::spawn(async move { stub_server().serve(io).await });

    drop(client_tx);
    let err = handle
        .await
        .expect("serve task")
        .expect_err("a closed client ends the handshake");
    assert_eq!(err.category, ErrorCategory::Unavailable);
    assert_eq!(err.message, "client closed before hello");
    drop(client_rx.next().await);
}

/// A tool that requires approval, so the run pauses at a permission ask.
struct ApprovableTool;

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
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async { Ok(serde_json::json!({"ok": true})) })
    }
    fn requires_approval(&self) -> bool {
        true
    }
}

/// A read-only tool that keeps a resumed run active until released.
struct BlockingTool {
    entered: Arc<Notify>,
    release: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
}

impl Tool for BlockingTool {
    fn name(&self) -> &str {
        "serve_loop_blocking"
    }
    fn description(&self) -> &str {
        "blocks a resumed run until the test releases it"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type":"object"})
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

/// A scripted model turn that asks for one tool call.
fn serve_loop_tool_call(id: &str, name: &str) -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    }
}

/// A scripted model turn that ends the run with plain text.
fn serve_loop_text(text: &str) -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::Text { text: text.into() }],
        usage: Usage::default(),
        model: "test".into(),
    }
}

/// Closing the connection after approving a permission ask, while the resumed
/// run is still active, ends the serve loop with a typed Unavailable error:
/// an abandoned resume is reported, not parked silently.
#[tokio::test]
async fn test_client_closed_during_resume() {
    let session = SessionId::new();
    let entered = Arc::new(Notify::new());
    let (release_tx, release_rx) = oneshot::channel();
    let release = Arc::new(Mutex::new(Some(release_rx)));
    let responses = vec![
        serve_loop_tool_call("toolu_approval", "approvable"),
        serve_loop_tool_call("toolu_blocking", "serve_loop_blocking"),
        serve_loop_text("never reached"),
    ];
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ApprovableTool));
    tools.register(Arc::new(BlockingTool {
        entered: entered.clone(),
        release,
    }));
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Arc::new(Runner::with_shared_store(
        store,
        Arc::new(houyicoder_provider::FakeProvider::new(responses)),
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 10,
            ..RunnerConfig::default()
        },
    ));
    let server = Server::new(runner, session, Arc::new(DefaultModeGate::new()));

    // Keep the receive half alive so event flushes succeed; dropping only
    // the send half makes the closed-client branch the one that fires.
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);

    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(
            RequestId(1),
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![houyicoder_protocol::frontend::run::ContentBlock::Text {
                    text: "go".into(),
                }],
                disabled_skills: Default::default(),
            },
        )),
    );

    // Read frames until the permission ask arrives, then approve it.
    let mut ask = None;
    while ask.is_none() {
        match recv_frame(&mut client_rx).await {
            ServerFrame::Request(req) => ask = Some(req),
            ServerFrame::Event(_) => {}
            other => panic!("expected the permission ask, got {other:?}"),
        }
    }
    let ask = ask.expect("permission ask");
    let call_id = match &ask.payload {
        ServerRequestPayload::Permission(a) => a.call_id.clone(),
        other => panic!("expected a permission ask, got {other:?}"),
    };
    send_line(
        &mut client_tx,
        &ClientFrame::Response(ClientResponseEnvelope::new(
            ask.req_id,
            ClientResponsePayload::Permission(ApprovalDecision {
                call_id,
                approved: true,
                updated_input: None,
                scope: "once".to_string(),
            }),
        )),
    );

    // Effect latch: the resumed run is inside the blocking tool, so the
    // resume loop is live when the send half closes.
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("resumed run entered the blocking tool");
    drop(client_tx);
    let err = handle
        .await
        .expect("serve task")
        .expect_err("closing during the resume ends the serve loop");
    assert_eq!(err.category, ErrorCategory::Unavailable);
    assert_eq!(err.message, "client closed mid-resume");
    drop(release_tx);
    drop(client_rx.next().await);
}
