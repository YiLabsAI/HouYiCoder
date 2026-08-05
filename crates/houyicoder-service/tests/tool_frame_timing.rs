//! Red tests for the mid-run tool-call frame (route B): the durable
//! ToolCall event must ship on the wire WHILE the run is still in flight
//! (before run_fut resolves), not only at an interruption or run terminal.
//!
//! Today the server pushes turn events only after run_fut resolves (the
//! outer loop's trajectory_snapshot + skip(pushed_count) + push_turn_event),
//! so a tool call is invisible until the run pauses on a permission ask or
//! ends. These tests pin the incremental-durable-push contract: push the
//! durable stream during the run, woken by the store's append Notify.
//!
//! The first test is RED today (no mid-run push); it goes GREEN once the
//! store fires notify_one on append and the serve select drains mid-run.
//! The second is a doubling guardrail (the call_id appears exactly once on
//! the wire for ToolCall and for ToolResult) that holds under both today's
//! behavior and route B, and fails if a mid-run push re-pushes at resolve.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use futures::channel::mpsc;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_client::{Client, InProcTransport};
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, StubTool, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::{EventEnvelope, RequestId, ServerFrame};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::frontend::FrontendEventKind;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::SessionUpdate;
use houyicoder_protocol::frontend::{FrontendRequest, SessionId as WireSessionId};
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use houyicoder_provider::FakeProvider;
use houyicoder_service::server::{Server, ServerIo};
use houyicoder_session::SessionStore;
use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::oneshot;

/// A read-only, approval-free tool whose execute blocks on a test-held
/// oneshot. On entry it fires a Notify (so the test knows the run reached
/// tool execution and the ToolCall event was already appended earlier in
/// append_response_events), then awaits the release receiver. Read-only +
/// requires_approval=false => Auto mode runs it without a permission ask, so
/// the run stays in flight at execute (not paused at an Interruption) — the
/// test's mid-run window is observable.
struct BlockingTool {
    blocked: Arc<Notify>,
    release: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
}

impl BlockingTool {
    fn new(blocked: Arc<Notify>, release: Arc<Mutex<Option<oneshot::Receiver<()>>>>) -> Self {
        Self { blocked, release }
    }
}

impl Tool for BlockingTool {
    fn name(&self) -> &str {
        "blocking"
    }
    fn description(&self) -> &str {
        "a read-only tool that blocks until the test releases it"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let blocked = self.blocked.clone();
        let release = self.release.clone();
        Box::pin(async move {
            blocked.notify_one();
            // Take the receiver out of the mutex before awaiting: a
            // std::sync::MutexGuard is not Send and cannot cross the await.
            let rx = {
                let mut guard = release.lock().expect("release mutex");
                guard.take()
            };
            if let Some(rx) = rx {
                let _ = rx.await;
            }
            Ok(serde_json::json!({"ok": true}))
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn is_destructive(&self) -> bool {
        false
    }
}

/// Whether the frame is a SessionUpdate::ToolCall with the given call id.
fn is_tool_call(frame: &ServerFrame, call_id: &str) -> bool {
    if let ServerFrame::Event(EventEnvelope {
        payload:
            FrontendEventKind::SessionUpdate {
                update: SessionUpdate::ToolCall(tc),
            },
        ..
    }) = frame
    {
        tc.tool_call_id.0.as_str() == call_id
    } else {
        false
    }
}

/// Whether the frame is a SessionUpdate::ToolCallUpdate (a tool result) with
/// the given call id.
fn is_tool_result(frame: &ServerFrame, call_id: &str) -> bool {
    if let ServerFrame::Event(EventEnvelope {
        payload:
            FrontendEventKind::SessionUpdate {
                update: SessionUpdate::ToolCallUpdate(upd),
            },
        ..
    }) = frame
    {
        upd.tool_call_id.0.as_str() == call_id
    } else {
        false
    }
}

fn count(frames: &[ServerFrame], pred: impl Fn(&ServerFrame) -> bool) -> usize {
    frames.iter().filter(|f| pred(f)).count()
}

/// Build a server + in-proc client, drive a MessageSend whose run uses the
/// given tools + scripted provider responses. Returns the client, the serve
/// task handle, and the request id the run is filed under. The run is in
/// flight when this returns; the caller drains frames from the client.
async fn spawn_server_with(
    tools: ToolRegistry,
    responses: Vec<CompletionResponse>,
) -> (Client, tokio::task::JoinHandle<()>, RequestId) {
    let notify = Arc::new(Notify::new());
    let store = Arc::new(
        SessionStore::new(Box::new(InMemoryBackend::new())).with_append_notify(notify.clone()),
    );
    let session = SessionId::new();
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(FakeProvider::new(responses));
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
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
    .with_append_notify(notify);
    let handle = tokio::spawn(async move {
        drop(server.serve(server_io).await);
    });
    client.connect().await.expect("handshake");
    let req_id = RequestId(1);
    client
        .send_request(
            req_id,
            FrontendRequest::MessageSend {
                session_id: WireSessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "go".to_string(),
                }],
            },
        )
        .await
        .expect("send message");
    (client, handle, req_id)
}

fn one_tool_call_response(call_id: &str, tool: &str) -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: call_id.into(),
            name: tool.into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    }
}

fn one_text_response(text: &str) -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::Text { text: text.into() }],
        usage: Usage::default(),
        model: "test".into(),
    }
}

/// Drive a run whose first response is a tool call to BlockingTool. The tool
/// blocks on a test-held oneshot, so the run stays in flight at execute.
/// While it is blocked, the ToolCall event must already be on the wire
/// (route B: the store's append Notify wakes the serve select to drain
/// mid-run). Today this is RED: no frame ships until run_fut resolves, so
/// the drain times out without seeing the ToolCall.
#[tokio::test]
async fn test_tool_frame_beats_resolve() {
    let blocked = Arc::new(Notify::new());
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let release = Arc::new(Mutex::new(Some(release_rx)));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BlockingTool::new(
        blocked.clone(),
        release.clone(),
    )));
    let (mut client, handle, req_id) = spawn_server_with(
        tools,
        vec![
            one_tool_call_response("toolu_1", "blocking"),
            one_text_response("done"),
        ],
    )
    .await;

    // Wait until the tool has entered execute: by then append_response_events
    // has already appended the ToolCall, so it is push-able (or not, today).
    blocked.notified().await;

    // Drain frames while the run is still in flight (the tool is blocked).
    // The ToolCall event must already be on the wire. A timeout continues
    // (not breaks) so a slow CI push is not missed; only a client error or a
    // frame that ends the window breaks. Today no frame ships until run_fut
    // resolves, so this loop exhausts its budget without the ToolCall (RED).
    let mut got_tool_call = false;
    let mut asked = false;
    for _ in 0..32 {
        match tokio::time::timeout(Duration::from_millis(100), client.next_frame()).await {
            Ok(Ok(frame)) => {
                if is_tool_call(&frame, "toolu_1") {
                    got_tool_call = true;
                    break;
                }
                if matches!(frame, ServerFrame::Request(_)) {
                    asked = true;
                    break;
                }
                if matches!(frame, ServerFrame::Response(_)) {
                    break;
                }
            }
            Ok(Err(_)) => break, // client channel closed
            Err(_) => continue,  // timeout: keep waiting for the mid-run push
        }
    }
    assert!(
        !asked,
        "BlockingTool asked for approval (setup fault): the run must reach \
         execute without a permission ask so the mid-run window is observable"
    );
    assert!(
        got_tool_call,
        "the ToolCall event must ship on the wire while the run is still in \
         flight, not only at run resolve"
    );

    // Release the tool so the run resumes to the final text reply.
    release_tx.send(()).expect("release the blocking tool");

    // Wait for the run to resolve (a Response for the MessageSend req_id).
    let mut ended = false;
    for _ in 0..128 {
        match tokio::time::timeout(Duration::from_secs(2), client.next_frame()).await {
            Ok(Ok(ServerFrame::Response(resp))) if resp.req_id == req_id => {
                ended = true;
                break;
            }
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    assert!(ended, "the run must resolve after the tool is released");

    drop(client);
    drop(handle.await);
}

/// Guardrail: a tool call's ToolCall and ToolResult frames each appear
/// exactly once on the wire across the whole run. Today this passes trivially
/// (one push of each at resolve). Route B must also pass: the mid-run push
/// advances pushed_count so the post-resolve drain skips already-pushed
/// frames. Fails if a mid-run push re-pushes at resolve (doubling) for either
/// the call or the result.
#[tokio::test]
async fn test_tool_frame_not_doubled() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(StubTool::new("blocking")));
    let (mut client, handle, req_id) = spawn_server_with(
        tools,
        vec![
            one_tool_call_response("toolu_2", "blocking"),
            one_text_response("done"),
        ],
    )
    .await;

    // Drain until the run resolves, collecting every frame.
    let mut frames: Vec<ServerFrame> = Vec::new();
    for _ in 0..256 {
        match tokio::time::timeout(Duration::from_secs(2), client.next_frame()).await {
            Ok(Ok(ServerFrame::Response(resp))) if resp.req_id == req_id => break,
            Ok(Ok(frame)) => frames.push(frame),
            _ => break,
        }
    }
    let calls = count(&frames, |f| is_tool_call(f, "toolu_2"));
    let results = count(&frames, |f| is_tool_result(f, "toolu_2"));
    assert_eq!(
        calls, 1,
        "the ToolCall frame must appear exactly once on the wire, got {calls} (doubling?): frames: {frames:?}"
    );
    assert_eq!(
        results, 1,
        "the ToolResult frame must appear exactly once on the wire, got {results} (doubling?): frames: {frames:?}"
    );

    drop(client);
    drop(handle.await);
}
