//! Delivery contracts for session events emitted during an active run.
//! Durable events must reach the frontend before completion and exactly once.

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
use houyicoder_protocol::acpx::AcpxMethod;
use houyicoder_protocol::envelope::{
    ClientResponsePayload, EventEnvelope, RequestId, ServerFrame, ServerRequestPayload,
};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalRequest, ContentBlock};
use houyicoder_protocol::frontend::session_update::SessionUpdate;
use houyicoder_protocol::frontend::{FrontendEvent, QueuedInput};
use houyicoder_protocol::frontend::{FrontendRequest, SessionId as WireSessionId};
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use houyicoder_provider::FakeProvider;
use houyicoder_service::server::{Server, ServerIo};
use houyicoder_session::SessionStore;
use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::oneshot;

/// A read-only tool that keeps the run active until released by the test.
struct BlockingTool {
    blocked: Arc<Notify>,
    release: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
}

struct SequencedBlockingTool {
    entered: Arc<Notify>,
    releases: Arc<Mutex<std::collections::VecDeque<oneshot::Receiver<()>>>>,
}

struct ApprovableTool;

impl Tool for ApprovableTool {
    fn name(&self) -> &str {
        "approvable"
    }
    fn description(&self) -> &str {
        "requires approval"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async { Ok(serde_json::json!({"ok": true})) })
    }
    fn requires_approval(&self) -> bool {
        true
    }
}

impl Tool for SequencedBlockingTool {
    fn name(&self) -> &str {
        "sequenced"
    }
    fn description(&self) -> &str {
        "blocks each invocation until the test releases it"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let entered = self.entered.clone();
        let receiver = self.releases.lock().expect("release queue").pop_front();
        Box::pin(async move {
            entered.notify_one();
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
            FrontendEvent::SessionUpdate {
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
fn is_user_message(frame: &ServerFrame, text: &str) -> bool {
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

fn is_tool_result(frame: &ServerFrame, call_id: &str) -> bool {
    if let ServerFrame::Event(EventEnvelope {
        payload:
            FrontendEvent::SessionUpdate {
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

fn is_text_delta(frame: &ServerFrame) -> bool {
    matches!(
        frame,
        ServerFrame::Event(EventEnvelope {
            payload: FrontendEvent::Acpx { notification },
            ..
        }) if notification.method == AcpxMethod::LlmTextDelta
    )
}

fn count(frames: &[ServerFrame], pred: impl Fn(&ServerFrame) -> bool) -> usize {
    frames.iter().filter(|f| pred(f)).count()
}

async fn collect_until_response(client: &mut Client, req_id: RequestId) -> Vec<ServerFrame> {
    let mut frames = Vec::new();
    for _ in 0..256 {
        match tokio::time::timeout(Duration::from_secs(2), client.next_frame()).await {
            Ok(Ok(ServerFrame::Response(response))) if response.req_id == req_id => break,
            Ok(Ok(frame)) => frames.push(frame),
            _ => break,
        }
    }
    frames
}

fn input_positions(frames: &[ServerFrame], input: &QueuedInput) -> (usize, usize) {
    let user = frames
        .iter()
        .position(|frame| is_user_message(frame, &input.text))
        .expect("user projection");
    let commit = frames
        .iter()
        .position(|frame| {
            matches!(
                frame,
                ServerFrame::Event(EventEnvelope {
                    payload: FrontendEvent::QueuedInputCommitted { inputs },
                    ..
                }) if inputs.iter().any(|committed| committed.id == input.id)
            )
        })
        .expect("queue commit");
    (user, commit)
}

fn assert_input_order(frames: &[ServerFrame], input: &QueuedInput) {
    let (user, commit) = input_positions(frames, input);
    let delta = frames.iter().position(is_text_delta).expect("model delta");
    assert!(user < commit && commit < delta, "{frames:?}");
    let seqs: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            ServerFrame::Event(event) => Some(event.seq),
            _ => None,
        })
        .collect();
    assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
}

async fn await_notification_dispatch(client: &mut Client) {
    let req_id = RequestId(u64::MAX - 1);
    client
        .send_request(
            req_id,
            FrontendRequest::ChildTranscript {
                child_sid: WireSessionId::new(SessionId::new().to_string()),
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
    let mut runner = Runner::with_shared_store(
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
    let event_sequencer = houyicoder_service::server::EventSequencer::new();
    event_sequencer.install_on(&mut runner);
    let server_io = ServerIo::new(server_tx, server_rx);
    let client_transport = InProcTransport::from_halves(client_tx, client_rx);
    let mut client = Client::new(Box::new(client_transport));
    let server = Server::new_with_event_sequencer(
        Arc::new(runner),
        session,
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
        event_sequencer,
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
                disabled_skills: Default::default(),
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

/// A tool call reaches the frontend while its run remains active.
#[tokio::test]
async fn test_tool_delivery() {
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

    // Keep polling through short scheduling delays while the tool holds the
    // run open. A response ends the observation window.
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

/// Tool call and result events are each delivered once.
#[tokio::test]
async fn test_event_dedup() {
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

/// A committed input leaves the queue before the active run completes.
#[tokio::test]
async fn test_input_commit() {
    let entered = Arc::new(Notify::new());
    let (release_first_tx, release_first_rx) = oneshot::channel();
    let (release_second_tx, release_second_rx) = oneshot::channel();
    let releases = Arc::new(Mutex::new(std::collections::VecDeque::from([
        release_first_rx,
        release_second_rx,
    ])));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SequencedBlockingTool {
        entered: entered.clone(),
        releases,
    }));
    let (mut client, handle, req_id) = spawn_server_with(
        tools,
        vec![
            one_tool_call_response("toolu_first", "sequenced"),
            one_tool_call_response("toolu_second", "sequenced"),
            one_text_response("done"),
        ],
    )
    .await;

    entered.notified().await;
    client
        .send_notification(houyicoder_protocol::acp_wire::AcpNotification::new(
            "session/inject",
            serde_json::json!({ "text": "mid note" }),
        ))
        .await
        .expect("send injected input");
    await_notification_dispatch(&mut client).await;
    release_first_tx.send(()).expect("release first tool");
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("second tool starts before the run resolves");

    let mut got_user = false;
    let mut got_committed = false;
    for _ in 0..64 {
        let frame = tokio::time::timeout(Duration::from_millis(100), client.next_frame())
            .await
            .expect("mid-run projection must not wait for completion")
            .expect("server frame");
        assert!(
            !matches!(frame, ServerFrame::Response(_)),
            "run resolved before the second tool was released"
        );
        got_user |= is_user_message(&frame, "mid note");
        got_committed |= matches!(
            frame,
            ServerFrame::Event(EventEnvelope {
                payload: FrontendEvent::QueuedInputCommitted { ref inputs },
                ..
            }) if inputs.iter().any(|input| input.text == "mid note")
        );
        if got_user && got_committed {
            break;
        }
    }
    assert!(
        got_user,
        "committed input reached the transcript before completion"
    );
    assert!(
        got_committed,
        "committed input left the queue before completion"
    );

    release_second_tx.send(()).expect("release second tool");
    for _ in 0..128 {
        if matches!(
            tokio::time::timeout(Duration::from_secs(2), client.next_frame()).await,
            Ok(Ok(ServerFrame::Response(ref response))) if response.req_id == req_id
        ) {
            break;
        }
    }
    drop(client);
    drop(handle.await);
}

/// A committed input is projected and retired before the next model delta.
#[tokio::test]
async fn test_input_order() {
    let blocked = Arc::new(Notify::new());
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let release = Arc::new(Mutex::new(Some(release_rx)));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BlockingTool::new(blocked.clone(), release)));
    let (mut client, handle, req_id) = spawn_server_with(
        tools,
        vec![
            one_tool_call_response("toolu_order", "blocking"),
            one_text_response("after input"),
        ],
    )
    .await;

    blocked.notified().await;
    let input = QueuedInput::new("ordered note");
    client
        .send_notification(houyicoder_protocol::acp_wire::AcpNotification::new(
            "session/inject",
            serde_json::json!({ "input": input }),
        ))
        .await
        .expect("send injected input");
    await_notification_dispatch(&mut client).await;
    release_tx.send(()).expect("release tool");

    let mut frames = Vec::new();
    for _ in 0..256 {
        match tokio::time::timeout(Duration::from_secs(2), client.next_frame()).await {
            Ok(Ok(ServerFrame::Response(response))) if response.req_id == req_id => break,
            Ok(Ok(frame)) => frames.push(frame),
            _ => break,
        }
    }
    assert_input_order(&frames, &input);

    drop(client);
    drop(handle.await);
}

/// Queued input keeps the same order after an approval resumes the run.
#[tokio::test]
async fn test_resume_orders_queued_input() {
    let blocked = Arc::new(Notify::new());
    let (release_tx, release_rx) = oneshot::channel();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ApprovableTool));
    tools.register(Arc::new(BlockingTool::new(
        blocked.clone(),
        Arc::new(Mutex::new(Some(release_rx))),
    )));
    let (mut client, handle, req_id) = spawn_server_with(
        tools,
        vec![
            one_tool_call_response("toolu_ask", "approvable"),
            one_tool_call_response("toolu_resume", "blocking"),
            one_text_response("after resume"),
        ],
    )
    .await;

    let ask = loop {
        if let ServerFrame::Request(ask) = client.next_frame().await.expect("permission request") {
            break ask;
        }
    };
    let call_id = match ask.payload {
        ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
        _ => panic!("expected permission request"),
    };
    client
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
        .expect("approve request");
    blocked.notified().await;
    let input = QueuedInput::new("resume note");
    client
        .send_notification(houyicoder_protocol::acp_wire::AcpNotification::new(
            "session/inject",
            serde_json::json!({ "input": input }),
        ))
        .await
        .expect("inject after resume");
    await_notification_dispatch(&mut client).await;
    release_tx.send(()).expect("release resumed tool");

    let frames = collect_until_response(&mut client, req_id).await;
    assert_input_order(&frames, &input);
    drop(client);
    drop(handle.await);
}

/// Multiple queued inputs retain FIFO order before the next model delta.
#[tokio::test]
async fn test_input_batch_preserves_fifo() {
    let blocked = Arc::new(Notify::new());
    let (release_tx, release_rx) = oneshot::channel();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BlockingTool::new(
        blocked.clone(),
        Arc::new(Mutex::new(Some(release_rx))),
    )));
    let (mut client, handle, req_id) = spawn_server_with(
        tools,
        vec![
            one_tool_call_response("toolu_batch", "blocking"),
            one_text_response("after batch"),
        ],
    )
    .await;

    blocked.notified().await;
    let inputs = [QueuedInput::new("first"), QueuedInput::new("second")];
    for input in &inputs {
        client
            .send_notification(houyicoder_protocol::acp_wire::AcpNotification::new(
                "session/inject",
                serde_json::json!({ "input": input }),
            ))
            .await
            .expect("inject queued input");
        await_notification_dispatch(&mut client).await;
    }
    release_tx.send(()).expect("release tool");

    let frames = collect_until_response(&mut client, req_id).await;
    assert_input_order(&frames, &inputs[0]);
    assert_input_order(&frames, &inputs[1]);
    let first = input_positions(&frames, &inputs[0]);
    let second = input_positions(&frames, &inputs[1]);
    let delta = frames.iter().position(is_text_delta).expect("model delta");
    assert!(first.0 < first.1 && first.1 < second.0, "{frames:?}");
    assert!(second.0 < second.1 && second.1 < delta, "{frames:?}");
    drop(client);
    drop(handle.await);
}
