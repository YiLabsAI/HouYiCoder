//! ACP serve-loop tests: the prompt happy path, the frame failures while a
//! run or resume is active, and the permission reverse-request failure
//! paths. The tests assert both the returned ProtocolError and the ACP error
//! response.

#![cfg(test)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::{PermissionVerdict, SessionEvent, SessionId};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::acp_wire::{
    AcpError, AcpErrorCode, AcpNotification, AcpRequest, AcpRequestId, AcpResponse, PromptResponse,
    RequestPermissionOutcome, RequestPermissionResponse, SelectedPermissionOutcome,
};
use houyicoder_protocol::acpx::AcpxCapabilities;
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::run::StopReason;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;

use super::AcpServer;
use crate::acp_adapter::AcpAdapter;
use crate::acp_serve::AcpIo;
use crate::lifecycle::SessionLeaseStore;

/// A tool that always needs a human verdict, so a run suspends at
/// Interruption and the server surfaces the permission reverse request.
struct GuardedTool;

impl Tool for GuardedTool {
    fn name(&self) -> &str {
        "guarded"
    }
    fn description(&self) -> &str {
        "a tool that needs approval"
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move { Ok(json!({"ok": true})) })
    }
    fn requires_approval(&self) -> bool {
        true
    }
}

fn text_response(text: &str) -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::Text { text: text.into() }],
        usage: Usage::default(),
        model: "test".into(),
    }
}

fn tool_call_response(id: &str, name: &str) -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: id.into(),
            name: name.into(),
            input: json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    }
}

fn runner_with(provider: FakeProvider, tools: ToolRegistry) -> Arc<Runner> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(provider);
    Arc::new(Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    ))
}

fn guarded_registry() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool));
    tools
}

fn guarded_runner() -> Arc<Runner> {
    runner_with(
        FakeProvider::new(vec![
            tool_call_response("toolu_1", "guarded"),
            text_response("done after approval"),
        ]),
        guarded_registry(),
    )
}

/// A read-only tool that keeps a run active until the test releases it or
/// the run's cancellation token fires, so tests order the run against ACP
/// frames by latch instead of a wall-clock margin.
struct BlockingTool {
    entered: Arc<Notify>,
    release: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
}

impl Tool for BlockingTool {
    fn name(&self) -> &str {
        "acp_blocking"
    }
    fn description(&self) -> &str {
        "blocks a run until the test releases it"
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object"})
    }
    fn execute(&self, ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let entered = self.entered.clone();
        let release = self.release.clone();
        Box::pin(async move {
            entered.notify_one();
            let receiver = release.lock().expect("release lock").take();
            let released = async {
                if let Some(receiver) = receiver {
                    let _ = receiver.await;
                }
            };
            match ctx.cancel {
                Some(token) => tokio::select! {
                    _ = released => {}
                    _ = token.cancelled() => {}
                },
                None => released.await,
            }
            Ok(json!({"ok": true}))
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
}

/// Build the blocking tool plus the latch handles the test drives: entered
/// proves the run is live inside the tool; release ends the block.
fn blocking_latch() -> (BlockingTool, Arc<Notify>, oneshot::Sender<()>) {
    let entered = Arc::new(Notify::new());
    let (release_tx, release_rx) = oneshot::channel();
    let tool = BlockingTool {
        entered: entered.clone(),
        release: Arc::new(Mutex::new(Some(release_rx))),
    };
    (tool, entered, release_tx)
}

/// A runner whose run blocks inside a tool after the first model call, so
/// the run stays active until the latch says otherwise.
fn blocking_runner() -> (Arc<Runner>, Arc<Notify>, oneshot::Sender<()>) {
    let (tool, entered, release_tx) = blocking_latch();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(tool));
    let runner = runner_with(
        FakeProvider::new(vec![
            tool_call_response("toolu_1", "acp_blocking"),
            text_response("late"),
        ]),
        tools,
    );
    (runner, entered, release_tx)
}

/// A runner that first parks at a permission ask and, once approved, blocks
/// inside a tool on the resumed run.
fn guarded_blocking_runner() -> (Arc<Runner>, Arc<Notify>, oneshot::Sender<()>) {
    let (tool, entered, release_tx) = blocking_latch();
    let mut tools = guarded_registry();
    tools.register(Arc::new(tool));
    let runner = runner_with(
        FakeProvider::new(vec![
            tool_call_response("toolu_1", "guarded"),
            tool_call_response("toolu_2", "acp_blocking"),
            text_response("late"),
        ]),
        tools,
    );
    (runner, entered, release_tx)
}

fn spawn_server(
    runner: Arc<Runner>,
    session: SessionId,
) -> (
    JoinHandle<Result<(), ProtocolError>>,
    mpsc::Sender<String>,
    mpsc::Receiver<String>,
) {
    let adapter = Arc::new(AcpAdapter::new(
        AcpxCapabilities::default(),
        1,
        SessionLeaseStore::new(),
    ));
    let srv = AcpServer::new(adapter, runner, session);
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let (server_tx, client_rx) = mpsc::channel::<String>(256);
    let mut io = AcpIo::new(server_tx, server_rx);
    let handle = tokio::spawn(async move { srv.serve(&mut io).await });
    (handle, client_tx, client_rx)
}

fn prompt_params(session: SessionId, text: &str) -> Value {
    json!({
        "sessionId": session.to_string(),
        "prompt": [{ "type": "text", "text": text }],
    })
}

fn prompt_request(session: SessionId, id: i64) -> AcpRequest {
    AcpRequest::new(id, "session/prompt", prompt_params(session, "go"))
}

async fn send(tx: &mut mpsc::Sender<String>, msg: &impl serde::Serialize) {
    let mut f = encode(msg).expect("encode");
    if !f.ends_with('\n') {
        f.push('\n');
    }
    tx.send(f).await.expect("send");
}

async fn recv(rx: &mut mpsc::Receiver<String>) -> String {
    tokio::time::timeout(Duration::from_secs(5), rx.next())
        .await
        .expect("server frame timeout")
        .expect("server replied")
        .trim_end()
        .to_string()
}

/// Read frames until the permission reverse request arrives and return it
/// typed, so the test can answer on the exact id the server minted.
async fn recv_ask(rx: &mut mpsc::Receiver<String>) -> AcpRequest {
    loop {
        let frame = recv(rx).await;
        if frame.contains("session/request_permission") {
            return serde_json::from_str(&frame).expect("ask parses");
        }
    }
}

async fn recv_prompt_result(rx: &mut mpsc::Receiver<String>, id: i64) -> Value {
    loop {
        let frame = recv(rx).await;
        if let Ok(AcpResponse::Result {
            id: rid, result, ..
        }) = serde_json::from_str::<AcpResponse>(&frame)
            && rid == AcpRequestId::Number(id)
        {
            return result;
        }
    }
}

async fn recv_acp_error(rx: &mut mpsc::Receiver<String>) -> AcpError {
    loop {
        let frame = recv(rx).await;
        if let Ok(AcpResponse::Error { error, .. }) = serde_json::from_str::<AcpResponse>(&frame) {
            return error;
        }
    }
}

fn allow_once(ask_id: AcpRequestId) -> AcpResponse {
    AcpResponse::ok(
        ask_id,
        serde_json::to_value(RequestPermissionResponse {
            outcome: RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                option_id: "allow_once".into(),
                meta: None,
            }),
            meta: None,
        })
        .expect("permission response serializes"),
    )
}

/// The prompt happy path: the run streams its turn events and the request id
/// is answered with a typed PromptResponse carrying the EndTurn stop reason.
#[tokio::test]
async fn test_prompt_final_output() {
    let session = SessionId::new();
    let runner = runner_with(FakeProvider::text("ok"), ToolRegistry::new());
    let (handle, mut tx, mut rx) = spawn_server(runner, session);

    send(&mut tx, &prompt_request(session, 7)).await;
    let result = recv_prompt_result(&mut rx, 7).await;
    let resp: PromptResponse = serde_json::from_value(result).expect("typed prompt response");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);

    drop(tx);
    handle.await.expect("join").expect("clean close");
}

/// A client that closes while the run is active fails the serve loop with a
/// typed Unavailable error; the best-effort protocol-error write to the dead
/// carrier is swallowed, and the original failure is what returns.
#[tokio::test]
async fn test_client_closed_during_run() {
    let session = SessionId::new();
    let (runner, entered, release_tx) = blocking_runner();
    let (handle, mut tx, rx) = spawn_server(runner, session);

    send(&mut tx, &prompt_request(session, 1)).await;
    // Effect latch: the run is live inside the tool, so the closed carrier
    // is observed by the frame branch of the run select, not raced.
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("run entered the blocking tool");
    drop(tx);
    drop(rx);
    let err = handle
        .await
        .expect("join")
        .expect_err("closing during a run fails the serve loop");
    assert_eq!(err.category, ErrorCategory::Unavailable);
    assert_eq!(err.message, "client closed mid-run");
    drop(release_tx);
}

/// Any non-cancel frame arriving while the run is active is a protocol
/// violation: the serve loop fails closed, and the client reads the exact
/// ACP error response — Display carries the message only, no category
/// prefix.
#[tokio::test]
async fn test_unexpected_frame_during_run() {
    let session = SessionId::new();
    let (runner, entered, release_tx) = blocking_runner();
    let (handle, mut tx, mut rx) = spawn_server(runner, session);

    send(&mut tx, &prompt_request(session, 1)).await;
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("run entered the blocking tool");
    send(&mut tx, &AcpRequest::new(5, "bogus", json!({}))).await;
    let err = handle
        .await
        .expect("join")
        .expect_err("a request frame during a run fails closed");
    assert_eq!(err.category, ErrorCategory::InvalidFrame);
    assert_eq!(err.message, "unexpected frame mid-run");

    let error = recv_acp_error(&mut rx).await;
    assert_eq!(error.message, "unexpected frame mid-run");
    drop(release_tx);
}

/// A session/cancel notification while the run is active routes to the abort
/// token: the run resolves cancelled and the prompt id is answered with the
/// Cancelled stop reason instead of hanging.
#[tokio::test]
async fn test_cancel_during_run_stops() {
    let session = SessionId::new();
    let (runner, entered, release_tx) = blocking_runner();
    let (handle, mut tx, mut rx) = spawn_server(runner, session);

    send(&mut tx, &prompt_request(session, 3)).await;
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("run entered the blocking tool");
    send(&mut tx, &AcpNotification::new("session/cancel", json!({}))).await;
    let result = recv_prompt_result(&mut rx, 3).await;
    let resp: PromptResponse = serde_json::from_value(result).expect("typed prompt response");
    assert_eq!(resp.stop_reason, StopReason::Cancelled);

    drop(tx);
    handle.await.expect("join").expect("clean close");
    drop(release_tx);
}

/// A client that closes while the permission ask is in flight fails the
/// serve loop with a typed Unavailable error naming the permission phase.
#[tokio::test]
async fn test_client_closed_during_permission() {
    let session = SessionId::new();
    let (handle, mut tx, mut rx) = spawn_server(guarded_runner(), session);

    send(&mut tx, &prompt_request(session, 1)).await;
    let _ask = recv_ask(&mut rx).await;
    drop(tx);
    drop(rx);
    let err = handle
        .await
        .expect("join")
        .expect_err("closing during the ask fails the serve loop");
    assert_eq!(err.category, ErrorCategory::Unavailable);
    assert_eq!(err.message, "client closed mid-permission");
}

/// A frame that parses as no JSON-RPC shape cannot answer the ask: the
/// serve loop fails closed with a typed InvalidFrame error.
#[tokio::test]
async fn test_garbage_permission_response() {
    let session = SessionId::new();
    let (handle, mut tx, mut rx) = spawn_server(guarded_runner(), session);

    send(&mut tx, &prompt_request(session, 1)).await;
    let _ask = recv_ask(&mut rx).await;
    tx.send("not json at all\n".into())
        .await
        .expect("send garbage");
    let err = handle
        .await
        .expect("join")
        .expect_err("a garbage answer fails closed");
    assert_eq!(err.category, ErrorCategory::InvalidFrame);
    assert!(!err.message.is_empty(), "the parse failure is carried");
}

/// An error response to the permission ask is a refusal the protocol cannot
/// proceed past: the serve loop fails closed and names the rejection.
#[tokio::test]
async fn test_rejected_permission_ask() {
    let session = SessionId::new();
    let (handle, mut tx, mut rx) = spawn_server(guarded_runner(), session);

    send(&mut tx, &prompt_request(session, 1)).await;
    let ask = recv_ask(&mut rx).await;
    send(
        &mut tx,
        &AcpResponse::err(
            ask.id.clone(),
            AcpErrorCode::InternalError,
            "client said no",
        ),
    )
    .await;
    let err = handle
        .await
        .expect("join")
        .expect_err("a rejected ask fails closed");
    assert_eq!(err.category, ErrorCategory::InvalidFrame);
    assert!(
        err.message.starts_with("permission ask rejected"),
        "the rejection is named: {}",
        err.message
    );
}

/// A success response whose result is not a permission outcome cannot be
/// guessed at: the serve loop fails closed with a typed InvalidFrame error.
#[tokio::test]
async fn test_bad_permission_result() {
    let session = SessionId::new();
    let (handle, mut tx, mut rx) = spawn_server(guarded_runner(), session);

    send(&mut tx, &prompt_request(session, 1)).await;
    let ask = recv_ask(&mut rx).await;
    send(
        &mut tx,
        &AcpResponse::ok(ask.id.clone(), json!({"nonsense": 1})),
    )
    .await;
    let err = handle
        .await
        .expect("join")
        .expect_err("a malformed outcome fails closed");
    assert_eq!(err.category, ErrorCategory::InvalidFrame);
    assert!(!err.message.is_empty(), "the decode failure is carried");
}

/// The permission happy path: an allow_once answer resumes the run to its
/// final output, and the approval verdict is durably audited in the session
/// trajectory before the resume applies it.
#[tokio::test]
async fn test_permission_allow_completes() {
    let session = SessionId::new();
    let runner = guarded_runner();
    let inspector = runner.clone();
    let (handle, mut tx, mut rx) = spawn_server(runner, session);

    send(&mut tx, &prompt_request(session, 4)).await;
    let ask = recv_ask(&mut rx).await;
    send(&mut tx, &allow_once(ask.id.clone())).await;
    let result = recv_prompt_result(&mut rx, 4).await;
    let resp: PromptResponse = serde_json::from_value(result).expect("typed prompt response");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);

    let events = inspector.store().trajectory_snapshot(session);
    assert!(
        events.iter().any(|e| matches!(
            e.event,
            SessionEvent::PermissionDecision {
                verdict: PermissionVerdict::Approved,
                ..
            }
        )),
        "the approval verdict is audited in the trajectory"
    );

    drop(tx);
    handle.await.expect("join").expect("clean close");
}

/// A non-cancel frame arriving while the resume is active is the same
/// protocol violation as during the run: the serve loop fails closed and the
/// client reads the exact ACP error response.
#[tokio::test]
async fn test_unexpected_frame_during_resume() {
    let session = SessionId::new();
    let (runner, entered, release_tx) = guarded_blocking_runner();
    let (handle, mut tx, mut rx) = spawn_server(runner, session);

    send(&mut tx, &prompt_request(session, 6)).await;
    let ask = recv_ask(&mut rx).await;
    send(&mut tx, &allow_once(ask.id.clone())).await;
    // Effect latch: the resumed run is live inside the tool, so the bogus
    // frame arrives while the resume select is still reading.
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("resumed run entered the blocking tool");
    send(&mut tx, &AcpRequest::new(9, "bogus", json!({}))).await;
    let err = handle
        .await
        .expect("join")
        .expect_err("a request frame during a resume fails closed");
    assert_eq!(err.category, ErrorCategory::InvalidFrame);
    assert_eq!(err.message, "unexpected frame mid-resume");

    let error = recv_acp_error(&mut rx).await;
    assert_eq!(error.message, "unexpected frame mid-resume");
    drop(release_tx);
}

/// A client that closes after answering the ask, while the resume is still
/// in flight, fails the serve loop with a typed Unavailable error naming
/// the resume phase.
#[tokio::test]
async fn test_client_closed_during_resume() {
    let session = SessionId::new();
    let (runner, entered, release_tx) = guarded_blocking_runner();
    let (handle, mut tx, mut rx) = spawn_server(runner, session);

    send(&mut tx, &prompt_request(session, 1)).await;
    let ask = recv_ask(&mut rx).await;
    send(&mut tx, &allow_once(ask.id.clone())).await;
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("resumed run entered the blocking tool");
    drop(tx);
    drop(rx);
    let err = handle
        .await
        .expect("join")
        .expect_err("closing during a resume fails the serve loop");
    assert_eq!(err.category, ErrorCategory::Unavailable);
    assert_eq!(err.message, "client closed mid-resume");
    drop(release_tx);
}
