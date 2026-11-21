//! ACP serve-loop tests: the prompt happy path, the frame failures while a
//! run or resume is active, and the permission reverse-request failure
//! paths. The tests assert both the returned ProtocolError and the ACP error
//! response.

#![cfg(test)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::stream;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_async::PStream;
use houyicoder_context::{PermissionVerdict, SessionEvent, SessionId};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::acp_wire::{
    AcpError, AcpErrorCode, AcpNotification, AcpRequest, AcpRequestId, AcpResponse, PromptResponse,
    RequestPermissionOutcome, RequestPermissionResponse, SelectedPermissionOutcome,
};
use houyicoder_protocol::acpx::AcpxCapabilities;
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::run::StopReason;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;
use tokio::time::sleep;

use super::AcpServer;
use crate::acp::adapter::AcpAdapter;
use crate::acp::live::AcpLiveStream;
use crate::acp::pump::from_line_channels;
use crate::lifecycle::SessionLeaseStore;
use tokio::time::timeout;

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
    Arc::new(Runner::new(
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
    let srv = AcpServer::new(adapter, runner, Arc::new(DefaultModeGate::new()), session);
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let (server_tx, client_rx) = mpsc::channel::<String>(256);
    let mut io = from_line_channels(server_rx, server_tx);
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

async fn send(tx: &mpsc::Sender<String>, msg: &impl serde::Serialize) {
    let mut f = encode(msg).expect("encode");
    if !f.ends_with('\n') {
        f.push('\n');
    }
    tx.send(f).await.expect("send");
}

async fn recv(rx: &mut mpsc::Receiver<String>) -> String {
    timeout(Duration::from_secs(5), rx.recv())
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

fn cancelled(ask_id: AcpRequestId) -> AcpResponse {
    AcpResponse::ok(
        ask_id,
        serde_json::to_value(RequestPermissionResponse {
            outcome: RequestPermissionOutcome::Cancelled,
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
    let (handle, tx, mut rx) = spawn_server(runner, session);

    send(&tx, &prompt_request(session, 7)).await;
    let result = recv_prompt_result(&mut rx, 7).await;
    let resp: PromptResponse = serde_json::from_value(result).expect("typed prompt response");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);

    drop(tx);
    handle.await.expect("join").expect("clean close");
}

/// The client's own message reaches it before the answer, exactly once: the
/// prompt echo goes out when the prompt arrives, and the durable copy of the
/// same record is suppressed.
#[tokio::test]
async fn test_prompt_echoes_user_once() {
    let session = SessionId::new();
    let runner = runner_with(FakeProvider::text("ok"), ToolRegistry::new());
    let (handle, tx, mut rx) = spawn_server(runner, session);

    send(&tx, &prompt_request(session, 4)).await;
    let mut updates = Vec::new();
    let mut user_text = String::new();
    loop {
        let frame = recv(&mut rx).await;
        let Ok(msg) = serde_json::from_str::<Value>(&frame) else {
            continue;
        };
        if msg.get("method") == Some(&json!("session/update")) {
            let update = &msg["params"];
            let kind = update["sessionUpdate"].as_str().unwrap_or_default();
            if kind == "user_message_chunk" {
                user_text.push_str(update["content"]["text"].as_str().unwrap_or_default());
            }
            updates.push(kind.to_string());
        }
        if msg.get("id") == Some(&json!(4)) {
            break;
        }
    }
    assert_eq!(
        updates.first().map(String::as_str),
        Some("user_message_chunk")
    );
    assert_eq!(
        updates
            .iter()
            .filter(|u| *u == "user_message_chunk")
            .count(),
        1,
        "the durable copy is suppressed: {updates:?}"
    );
    assert_eq!(user_text, "go");

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
    let (handle, tx, rx) = spawn_server(runner, session);

    send(&tx, &prompt_request(session, 1)).await;
    // Effect latch: the run is live inside the tool, so the closed carrier
    // is observed by the frame branch of the run select, not raced.
    timeout(Duration::from_secs(5), entered.notified())
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
    let (handle, tx, mut rx) = spawn_server(runner, session);

    send(&tx, &prompt_request(session, 1)).await;
    timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("run entered the blocking tool");
    send(&tx, &AcpRequest::new(5, "bogus", json!({}))).await;
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
    let (handle, tx, mut rx) = spawn_server(runner, session);

    send(&tx, &prompt_request(session, 3)).await;
    timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("run entered the blocking tool");
    send(&tx, &AcpNotification::new("session/cancel", json!({}))).await;
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
    let (handle, tx, mut rx) = spawn_server(guarded_runner(), session);

    send(&tx, &prompt_request(session, 1)).await;
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

/// A frame that parses as no JSON-RPC shape is answered with a parse error
/// and leaves the card pending; the real answer still decides the ask.
#[tokio::test]
async fn test_garbage_permission_response() {
    let session = SessionId::new();
    let (handle, tx, mut rx) = spawn_server(guarded_runner(), session);

    send(&tx, &prompt_request(session, 1)).await;
    let ask = recv_ask(&mut rx).await;
    tx.send("not json at all\n".into())
        .await
        .expect("send garbage");
    let error = recv_acp_error(&mut rx).await;
    assert_eq!(error.code, AcpErrorCode::ParseError);
    send(&tx, &allow_once(ask.id.clone())).await;
    let result = recv_prompt_result(&mut rx, 1).await;
    let resp: PromptResponse = serde_json::from_value(result).expect("typed prompt response");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);

    drop(tx);
    handle.await.expect("join").expect("clean close");
}

/// An error response to the permission ask is a refusal the protocol cannot
/// proceed past: the serve loop fails closed and names the rejection.
#[tokio::test]
async fn test_rejected_permission_ask() {
    let session = SessionId::new();
    let (handle, tx, mut rx) = spawn_server(guarded_runner(), session);

    send(&tx, &prompt_request(session, 1)).await;
    let ask = recv_ask(&mut rx).await;
    send(
        &tx,
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
    let (handle, tx, mut rx) = spawn_server(guarded_runner(), session);

    send(&tx, &prompt_request(session, 1)).await;
    let ask = recv_ask(&mut rx).await;
    send(
        &tx,
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

/// A prompt with an image block is rejected with a named error, not silently
/// dropped: the agent declared promptCapabilities.image false at initialize.
#[tokio::test]
async fn test_image_block_rejected() {
    let session = SessionId::new();
    let (handle, tx, mut rx) = spawn_server(guarded_runner(), session);
    let prompt = json!({
        "sessionId": session.to_string(),
        "prompt": [{"type": "image", "data": "base64", "mimeType": "image/png"}],
    });
    send(&tx, &AcpRequest::new(1, "session/prompt", prompt)).await;
    let reply = recv(&mut rx).await;
    assert!(reply.contains(r#""id":1"#), "{reply}");
    assert!(reply.contains(r#""code":-32602"#), "{reply}");
    assert!(reply.contains("image content is not supported"), "{reply}");
    drop(tx);
    handle.await.expect("join").expect("clean close");
}

/// A resource_link attachment (an IDE @file) is rejected with a named error
/// instead of failing opaquely at deserialization.
#[tokio::test]
async fn test_resource_link_rejected() {
    let session = SessionId::new();
    let (handle, tx, mut rx) = spawn_server(guarded_runner(), session);
    let prompt = json!({
        "sessionId": session.to_string(),
        "prompt": [{"type": "resource_link", "uri": "file:///x", "name": "x"}],
    });
    send(&tx, &AcpRequest::new(1, "session/prompt", prompt)).await;
    let reply = recv(&mut rx).await;
    assert!(reply.contains(r#""code":-32602"#), "{reply}");
    assert!(
        reply.contains("resource_link attachments are not supported"),
        "{reply}"
    );
    drop(tx);
    handle.await.expect("join").expect("clean close");
}

/// session/set_mode applies the mode to the gate and replies with the
/// resulting state, so the client sees the current mode it must display.
#[tokio::test]
async fn test_set_mode_changes_gate() {
    let session = SessionId::new();
    let (handle, tx, mut rx) = spawn_server(guarded_runner(), session);
    let params = json!({"sessionId": session.to_string(), "modeId": "manual"});
    send(&tx, &AcpRequest::new(1, "session/set_mode", params)).await;
    // The reply body is empty; the mode change arrives as a session/update
    // current_mode_update notification before the reply.
    let notif = recv(&mut rx).await;
    assert!(notif.contains(r#""currentModeId":"manual""#), "{notif}");
    let reply = recv(&mut rx).await;
    assert!(reply.contains(r#""id":1"#), "{reply}");
    // An unknown mode id is refused, not silently applied.
    let bad = json!({"sessionId": session.to_string(), "modeId": "bogus"});
    send(&tx, &AcpRequest::new(2, "session/set_mode", bad)).await;
    let bad_reply = recv(&mut rx).await;
    assert!(bad_reply.contains(r#""code":-32602"#), "{bad_reply}");
    assert!(bad_reply.contains("unknown mode"), "{bad_reply}");
    // A wrong session id is rejected, not silently applied to this session.
    let wrong = json!({"sessionId": "wrong", "modeId": "manual"});
    send(&tx, &AcpRequest::new(3, "session/set_mode", wrong)).await;
    let wrong_reply = recv(&mut rx).await;
    assert!(wrong_reply.contains(r#""code":-32602"#), "{wrong_reply}");
    assert!(wrong_reply.contains("session id mismatch"), "{wrong_reply}");
    // Malformed params (modeId of the wrong type) report a decode error.
    let malformed = json!({"sessionId": session.to_string(), "modeId": 123});
    send(&tx, &AcpRequest::new(4, "session/set_mode", malformed)).await;
    let malformed_reply = recv(&mut rx).await;
    assert!(
        malformed_reply.contains(r#""code":-32602"#),
        "{malformed_reply}"
    );
    assert!(malformed_reply.contains("bad params"), "{malformed_reply}");
    drop(tx);
    handle.await.expect("join").expect("clean close");
}

/// session/set_config_option for the model option switches the runner's
/// active model when the value is in the catalog; an unknown id is refused.
#[tokio::test]
async fn test_set_config_option_switches() {
    let session = SessionId::new();
    let runner = guarded_runner();
    let inspector = runner.clone();
    let adapter = Arc::new(AcpAdapter::new(
        AcpxCapabilities::default(),
        1,
        SessionLeaseStore::new(),
    ));
    let server = AcpServer::new(adapter, runner, Arc::new(DefaultModeGate::new()), session)
        .with_model_options(vec!["test".into(), "alt".into()]);
    let (tx, server_rx) = mpsc::channel::<String>(256);
    let (server_tx, mut rx) = mpsc::channel::<String>(256);
    let mut io = from_line_channels(server_rx, server_tx);
    let handle = tokio::spawn(async move { server.serve(&mut io).await });

    let params = json!({
        "sessionId": session.to_string(),
        "configId": "model",
        "value": "alt",
    });
    send(
        &tx,
        &AcpRequest::new(1, "session/set_config_option", params),
    )
    .await;
    let reply = recv(&mut rx).await;
    assert!(reply.contains(r#""id":1"#), "{reply}");
    assert!(reply.contains(r#""configOptions""#), "{reply}");
    assert!(reply.contains(r#""currentValue":"alt""#), "{reply}");
    assert_eq!(inspector.active_model(), "alt");

    // An unknown model id is refused.
    let bad = json!({
        "sessionId": session.to_string(),
        "configId": "model",
        "value": "nope",
    });
    send(&tx, &AcpRequest::new(2, "session/set_config_option", bad)).await;
    let bad_reply = recv(&mut rx).await;
    assert!(bad_reply.contains(r#""code":-32602"#), "{bad_reply}");
    assert!(bad_reply.contains("model not in catalog"), "{bad_reply}");

    // An unknown config option id is refused.
    let unknown = json!({"sessionId": session.to_string(), "configId": "theme", "value": "dark"});
    send(
        &tx,
        &AcpRequest::new(3, "session/set_config_option", unknown),
    )
    .await;
    let unknown_reply = recv(&mut rx).await;
    assert!(
        unknown_reply.contains(r#""code":-32602"#),
        "{unknown_reply}"
    );
    assert!(
        unknown_reply.contains("unknown config option"),
        "{unknown_reply}"
    );

    // A boolean value for the model option is the wrong shape.
    let boolean = json!({"sessionId": session.to_string(), "configId": "model", "type": "boolean", "value": true});
    send(
        &tx,
        &AcpRequest::new(4, "session/set_config_option", boolean),
    )
    .await;
    let bool_reply = recv(&mut rx).await;
    assert!(bool_reply.contains(r#""code":-32602"#), "{bool_reply}");
    assert!(
        bool_reply.contains("value id, not a boolean"),
        "{bool_reply}"
    );

    drop(tx);
    handle.await.expect("join").expect("clean close");
}

/// A provider that streams text deltas with a small sleep between them, so
/// the run future is pending between deltas and the serve loop's live-delta
/// branch polls. A synchronous stream resolves in one poll and starves the
/// branch; the delay is what makes the live path observable.
struct StreamingProvider {
    text: String,
}

impl ModelProvider for StreamingProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let text = self.text.clone();
        Box::pin(async move {
            Ok(CompletionResponse {
                output: vec![OutputItem::Text { text }],
                usage: Usage::default(),
                model: "test".into(),
            })
        })
    }
    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let id = "t1".to_string();
        let parts: Vec<String> = vec!["hel".into(), "lo".into()];
        Box::pin(stream::unfold(
            (0u32, id, parts),
            |(i, id, parts)| async move {
                match i {
                    0 => Some((Ok(LlmEvent::StepStart { index: 0 }), (1, id, parts))),
                    1 => Some((Ok(LlmEvent::TextStart { id: id.clone() }), (2, id, parts))),
                    2 => {
                        sleep(Duration::from_millis(2)).await;
                        Some((
                            Ok(LlmEvent::TextDelta {
                                id: id.clone(),
                                text: parts[0].clone(),
                            }),
                            (3, id, parts),
                        ))
                    }
                    3 => {
                        sleep(Duration::from_millis(2)).await;
                        Some((
                            Ok(LlmEvent::TextDelta {
                                id: id.clone(),
                                text: parts[1].clone(),
                            }),
                            (4, id, parts),
                        ))
                    }
                    4 => Some((Ok(LlmEvent::TextEnd { id: id.clone() }), (5, id, parts))),
                    5 => Some((
                        Ok(LlmEvent::StepFinish {
                            index: 0,
                            reason: "stop".into(),
                            usage: None,
                        }),
                        (6, id, parts),
                    )),
                    _ => None,
                }
            },
        ))
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A live-streamed turn forwards response deltas as session/update chunks
/// during the run and suppresses the durable copy, so the client sees the
/// deltas, not the delta plus the assembled message.
#[tokio::test]
async fn test_live_stream_sends_deltas() {
    let session = SessionId::new();
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(StreamingProvider {
        text: "hello".into(),
    });
    let mut runner = Runner::new(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    let (sink, live) = AcpLiveStream::channel();
    sink.install_on(&mut runner);
    let runner = Arc::new(runner);
    let adapter = Arc::new(AcpAdapter::new(
        AcpxCapabilities::default(),
        1,
        SessionLeaseStore::new(),
    ));
    let server = AcpServer::new(adapter, runner, Arc::new(DefaultModeGate::new()), session)
        .with_live_stream(live);
    let (tx, server_rx) = mpsc::channel::<String>(256);
    let (server_tx, mut rx) = mpsc::channel::<String>(256);
    let mut io = from_line_channels(server_rx, server_tx);
    let handle = tokio::spawn(async move { server.serve(&mut io).await });

    send(&tx, &prompt_request(session, 1)).await;
    let mut chunk_lines: Vec<String> = Vec::new();
    let mut reply = None;
    for _ in 0..64 {
        let line = recv(&mut rx).await;
        if line.contains(r#""id":1"#) {
            reply = Some(line);
            break;
        }
        if line.contains("agent_message_chunk") {
            chunk_lines.push(line);
        }
    }
    let reply = reply.expect("got a prompt reply");
    assert!(reply.contains(r#""stopReason":"end_turn""#), "{reply}");
    // Both live deltas reach the client, including the tail that arrives in
    // the run's final frame; the durable full message is suppressed, so no
    // chunk carries the assembled text. This separates the live path from a
    // durable-only false green and proves the tail-drain fix.
    assert!(
        chunk_lines.iter().any(|l| l.contains(r#""text":"hel""#)),
        "the first live partial reached the client: {chunk_lines:?}"
    );
    assert!(
        chunk_lines.iter().any(|l| l.contains(r#""text":"lo""#)),
        "the tail live partial reached the client: {chunk_lines:?}"
    );
    assert!(
        !chunk_lines.iter().any(|l| l.contains(r#""text":"hello""#)),
        "the durable full message was suppressed: {chunk_lines:?}"
    );

    drop(tx);
    handle.await.expect("join").expect("clean close");
}

/// The permission happy path: an allow_once answer resumes the run to its
/// final output, and the approval verdict is durably audited in the session
/// trajectory before the resume applies it.
#[tokio::test]
async fn test_permission_allow_completes() {
    let session = SessionId::new();
    let runner = guarded_runner();
    let inspector = runner.clone();
    let (handle, tx, mut rx) = spawn_server(runner, session);

    send(&tx, &prompt_request(session, 4)).await;
    let ask = recv_ask(&mut rx).await;
    send(&tx, &allow_once(ask.id.clone())).await;
    let result = recv_prompt_result(&mut rx, 4).await;
    let resp: PromptResponse = serde_json::from_value(result).expect("typed prompt response");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);

    let events = inspector.store().trajectory_tail(session);
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

/// A session/cancel that arrives while the card is up ends the turn as
/// cancelled: the client never sends a verdict, so the ask is abandoned and
/// the prompt replies with the cancelled stop reason.
#[tokio::test]
async fn test_ask_cancel_ends_turn() {
    let session = SessionId::new();
    let runner = guarded_runner();
    let inspector = runner.clone();
    let (handle, tx, mut rx) = spawn_server(runner, session);

    send(&tx, &prompt_request(session, 5)).await;
    let _ask = recv_ask(&mut rx).await;
    let cancel = AcpNotification::new("session/cancel", json!({"sessionId": session.to_string()}));
    send(&tx, &cancel).await;

    let result = recv_prompt_result(&mut rx, 5).await;
    let resp: PromptResponse = serde_json::from_value(result).expect("typed prompt response");
    assert_eq!(resp.stop_reason, StopReason::Cancelled);

    let approvals = inspector.store().trajectory_head(session).summary.approvals;
    assert_eq!(approvals.cards, 1, "the question still reached the client");
    assert_eq!(approvals.unknown_outcome, 1, "and gave no verdict");

    drop(tx);
    handle.await.expect("join").expect("clean close");
}

/// A response addressed to a different ask id does not decide the pending
/// card: the wait keeps going until the answer for this ask arrives.
#[tokio::test]
async fn test_mismatched_ask_response_ignored() {
    let session = SessionId::new();
    let (handle, tx, mut rx) = spawn_server(guarded_runner(), session);

    send(&tx, &prompt_request(session, 3)).await;
    let ask = recv_ask(&mut rx).await;
    let wrong = allow_once(AcpRequestId::Number(999));
    send(&tx, &wrong).await;
    send(&tx, &allow_once(ask.id.clone())).await;

    let result = recv_prompt_result(&mut rx, 3).await;
    let resp: PromptResponse = serde_json::from_value(result).expect("typed prompt response");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);

    drop(tx);
    handle.await.expect("join").expect("clean close");
}

/// A verdict delivered an instant before the client closes still decides
/// the card: the biased select lets the answer win over the concurrent EOF,
/// so the approval is audited before the connection tears down.
#[tokio::test]
async fn test_close_after_answer_audits() {
    let session = SessionId::new();
    let runner = guarded_runner();
    let inspector = runner.clone();
    let (handle, tx, mut rx) = spawn_server(runner, session);

    send(&tx, &prompt_request(session, 7)).await;
    let ask = recv_ask(&mut rx).await;
    send(&tx, &allow_once(ask.id.clone())).await;
    drop(tx);
    drop(rx);
    drop(handle.await);
    let events = inspector.store().trajectory_tail(session);
    assert!(
        events.iter().any(|e| matches!(
            e.event,
            SessionEvent::PermissionDecision {
                verdict: PermissionVerdict::Approved,
                ..
            }
        )),
        "the answer was applied before the close"
    );
}

/// A cancel that ended one turn does not poison the next: the sticky flag
/// resets at prompt start, so a following prompt runs to its normal outcome.
#[tokio::test]
async fn test_prompt_after_cancel_runs() {
    let session = SessionId::new();
    let (handle, tx, mut rx) = spawn_server(guarded_runner(), session);

    send(&tx, &prompt_request(session, 1)).await;
    let _ask = recv_ask(&mut rx).await;
    let cancel = AcpNotification::new("session/cancel", json!({"sessionId": session.to_string()}));
    send(&tx, &cancel).await;
    let first = recv_prompt_result(&mut rx, 1).await;
    let resp: PromptResponse = serde_json::from_value(first).expect("typed prompt response");
    assert_eq!(resp.stop_reason, StopReason::Cancelled);

    send(&tx, &prompt_request(session, 2)).await;
    let second = recv_prompt_result(&mut rx, 2).await;
    let resp: PromptResponse = serde_json::from_value(second).expect("typed prompt response");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);

    drop(tx);
    handle.await.expect("join").expect("clean close");
}

/// The idle frame loop routes each shape: a request delegates to the
/// adapter, a malformed line is answered with a parse error, an orphan
/// response is logged without a reply, and a cancel notification aborts the
/// runner. The initialize reply at the end proves the loop kept serving.
#[tokio::test]
async fn test_idle_frames_answered() {
    let session = SessionId::new();
    let (handle, tx, mut rx) = spawn_server(guarded_runner(), session);

    tx.send("not json at all\n".into())
        .await
        .expect("send garbage");
    let error = recv_acp_error(&mut rx).await;
    assert_eq!(error.code, AcpErrorCode::ParseError);

    send(&tx, &AcpResponse::ok(AcpRequestId::Number(99), json!({}))).await;
    send(
        &tx,
        &AcpNotification::new("session/cancel", json!({"sessionId": session.to_string()})),
    )
    .await;
    send(
        &tx,
        &AcpRequest::new(9, "initialize", serde_json::json!({})),
    )
    .await;
    let reply = recv(&mut rx).await;
    assert!(reply.contains(r#""id":9"#), "{reply}");

    drop(tx);
    handle.await.expect("join").expect("clean close");
}

/// A request that arrives while a permission card is up is a protocol
/// violation: the card cannot be answered by a new request, so the serve
/// loop fails closed instead of guessing.
#[tokio::test]
async fn test_ask_request_fails_closed() {
    let session = SessionId::new();
    let (handle, tx, mut rx) = spawn_server(guarded_runner(), session);

    send(&tx, &prompt_request(session, 1)).await;
    let _ask = recv_ask(&mut rx).await;
    send(
        &tx,
        &AcpRequest::new(9, "initialize", serde_json::json!({})),
    )
    .await;
    let err = handle
        .await
        .expect("join")
        .expect_err("a request during the ask fails closed");
    assert_eq!(err.category, ErrorCategory::InvalidFrame);
    assert!(
        err.message.contains("permission ask is pending"),
        "the violation is named: {}",
        err.message
    );
}

/// An ask the client abandons is still a card on the durable trail: the
/// question reached the client, so the audit holds it with no verdict rather
/// than reporting that nothing was ever asked.
#[tokio::test]
async fn test_abandoned_ask_counts_card() {
    let session = SessionId::new();
    let runner = guarded_runner();
    let inspector = runner.clone();
    let (handle, tx, mut rx) = spawn_server(runner, session);

    send(&tx, &prompt_request(session, 1)).await;
    let _ask = recv_ask(&mut rx).await;
    drop(tx);
    drop(rx);
    let failed = handle.await.expect("join");
    assert!(
        failed.is_err(),
        "closing during the ask fails the serve loop"
    );

    let approvals = inspector.store().trajectory_head(session).summary.approvals;
    assert_eq!(approvals.cards, 1, "the client received the question");
    assert_eq!(approvals.unknown_outcome, 1, "and gave no verdict");
    assert_eq!(approvals.denied, 0, "abandoning a card is not a refusal");
}

/// A non-cancel frame arriving while the resume is active is the same
/// protocol violation as during the run: the serve loop fails closed and the
/// client reads the exact ACP error response.
#[tokio::test]
async fn test_unexpected_frame_during_resume() {
    let session = SessionId::new();
    let (runner, entered, release_tx) = guarded_blocking_runner();
    let (handle, tx, mut rx) = spawn_server(runner, session);

    send(&tx, &prompt_request(session, 6)).await;
    let ask = recv_ask(&mut rx).await;
    send(&tx, &allow_once(ask.id.clone())).await;
    // Effect latch: the resumed run is live inside the tool, so the bogus
    // frame arrives while the resume select is still reading.
    timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("resumed run entered the blocking tool");
    send(&tx, &AcpRequest::new(9, "bogus", json!({}))).await;
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
    let (handle, tx, mut rx) = spawn_server(runner, session);

    send(&tx, &prompt_request(session, 1)).await;
    let ask = recv_ask(&mut rx).await;
    send(&tx, &allow_once(ask.id.clone())).await;
    timeout(Duration::from_secs(5), entered.notified())
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

/// A cancelled ask is a card nobody answered: the run still stops at the
/// veto, while the audit holds the question as an unknown outcome rather
/// than reporting a denial the client never stated.
#[tokio::test]
async fn test_cancelled_ask_counts_unknown() {
    let session = SessionId::new();
    let runner = guarded_runner();
    let inspector = runner.clone();
    let (handle, tx, mut rx) = spawn_server(runner, session);

    send(&tx, &prompt_request(session, 1)).await;
    let ask = recv_ask(&mut rx).await;
    send(&tx, &cancelled(ask.id.clone())).await;
    drop(tx);
    drop(rx);
    let _served = handle.await.expect("join");

    let approvals = inspector.store().trajectory_head(session).summary.approvals;
    assert_eq!(approvals.cards, 1, "the client received the question");
    assert_eq!(
        approvals.unknown_outcome, 1,
        "a cancelled card states no outcome"
    );
    assert_eq!(approvals.denied, 0, "cancelling is not refusing");
}
