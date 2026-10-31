//! Dollar-mode direct shell dispatch: the command runs through the host's
//! sandbox path with no model request, its result lands in the session log,
//! and a cancel that arrives while it runs stops it.

#![cfg(test)]

use super::super::*;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::{
    ClientFrame, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::SessionId as FrontendSessionId;
use houyicoder_protocol::frontend::event::FrontendEvent;
use houyicoder_protocol::frontend::session_update::SessionUpdate;
use houyicoder_protocol::frontend::session_update::ToolCallUpdate;
use houyicoder_protocol::handshake::Hello;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use serde_json::Value;
use serde_json::json;
use std::future::pending;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// How long a reply may take before the test reports the host as stuck. A
/// host that awaits the command inline never answers the cancel request, so
/// this bound is what turns that defect into a failure rather than a hang.
const REPLY_BUDGET: Duration = Duration::from_secs(5);

/// Stands in for the sandbox shell: answers what it was asked to run and
/// counts the times it ran.
struct EchoShell {
    calls: Arc<AtomicUsize>,
}

impl Tool for EchoShell {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "stands in for the sandbox shell"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let calls = self.calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            let command = input.get("command").and_then(Value::as_str).unwrap_or("");
            Ok(json!({ "stdout": format!("ran {command}"), "success": true }))
        })
    }
}

/// Starts, then never returns: only the cancel race can end the call.
struct HangingShell {
    started: Mutex<Option<oneshot::Sender<()>>>,
}

impl Tool for HangingShell {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "stands in for a shell command that does not finish"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let started = self.started.lock().expect("started slot").take();
        Box::pin(async move {
            if let Some(report) = started {
                let _ = report.send(());
            }
            pending::<()>().await;
            Ok(Value::Null)
        })
    }
}

/// The tool-call update a frame carries, if the frame reports a call's
/// result. The acknowledgement ordering test uses this to find the frame that
/// hands the client the command's output.
fn tool_result_update(frame: &ServerFrame) -> Option<&ToolCallUpdate> {
    match frame {
        ServerFrame::Event(event) => match &event.payload {
            FrontendEvent::SessionUpdate {
                update: SessionUpdate::ToolCallUpdate(update),
            } => Some(update),
            _ => None,
        },
        _ => None,
    }
}

fn send_line(tx: &mut mpsc::Sender<String>, frame: &impl serde::Serialize) {
    let mut s = encode(frame).unwrap();
    if !s.ends_with('\n') {
        s.push('\n');
    }
    tx.try_send(s).unwrap();
}

async fn recv_frame(rx: &mut mpsc::Receiver<String>) -> ServerFrame {
    let line = timeout(REPLY_BUDGET, rx.next())
        .await
        .expect("the host answers within the reply budget")
        .expect("the connection stays open");
    serde_json::from_str(&line).expect("frame decodes")
}

/// A serving host over a channel carrier, held open for a whole test so the
/// same connection issues several requests against one log.
struct Host {
    runner: Arc<Runner>,
    session: SessionId,
    client_tx: mpsc::Sender<String>,
    client_rx: mpsc::Receiver<String>,
    serving: JoinHandle<Result<(), ProtocolError>>,
}

impl Host {
    async fn start(tool: Option<Arc<dyn Tool>>) -> Self {
        let session = SessionId::new();
        let mut tools = ToolRegistry::new();
        if let Some(tool) = tool {
            tools.register(tool);
        }
        let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let runner = Arc::new(Runner::new(
            store,
            // An empty reply sequence panics on any model call, so a test that
            // reaches the model fails rather than quietly spending a turn.
            Arc::new(FakeProvider::new(Vec::new())),
            tools,
            RunnerConfig::default(),
        ));
        let server = Server::new(
            Arc::clone(&runner),
            session,
            Arc::new(DefaultModeGate::new()),
        );
        let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
        let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
        let io = FrameCarrier::new(server_tx, server_rx);
        let serving = tokio::spawn(async move { server.serve(io).await });
        send_line(&mut client_tx, &Hello::local());
        drop(client_rx.next().await);
        Self {
            runner,
            session,
            client_tx,
            client_rx,
            serving,
        }
    }

    fn shell(&mut self, id: u64, command: &str) {
        let payload = FrontendRequest::DirectShellRun {
            session_id: FrontendSessionId::new(self.session.to_string()),
            command: command.into(),
        };
        send_line(
            &mut self.client_tx,
            &ClientFrame::Request(RequestEnvelope::new(RequestId(id), payload)),
        );
    }

    fn cancel(&mut self, id: u64) {
        let payload = FrontendRequest::RunCancel {
            session_id: FrontendSessionId::new(self.session.to_string()),
            reason: "test cancel".into(),
        };
        send_line(
            &mut self.client_tx,
            &ClientFrame::Request(RequestEnvelope::new(RequestId(id), payload)),
        );
    }

    /// The next reply payload. Event and request frames interleave with the
    /// replies while the command runs, so they are skipped here.
    async fn reply(&mut self) -> (RequestId, ResponsePayload) {
        loop {
            if let ServerFrame::Response(response) = recv_frame(&mut self.client_rx).await {
                return (response.req_id, response.payload);
            }
        }
    }

    /// Every frame up to and including the reply, in arrival order, so a test
    /// can assert what the client sees before the acknowledgement.
    async fn ordered_frames(&mut self) -> Vec<ServerFrame> {
        let mut frames = Vec::new();
        loop {
            let frame = recv_frame(&mut self.client_rx).await;
            let reply = matches!(frame, ServerFrame::Response(_));
            frames.push(frame);
            if reply {
                return frames;
            }
        }
    }

    async fn results(&self) -> Vec<Value> {
        let events = self
            .runner
            .store()
            .replay(self.session)
            .await
            .expect("replay");
        events
            .iter()
            .filter_map(|entry| match &entry.event {
                SessionEvent::ToolResult { output, .. } => Some(output.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

/// A typed command is answered with an acknowledgement, and the log holds the
/// call with its result and no model turn.
#[tokio::test]
async fn test_command_records_transcript() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut host = Host::start(Some(Arc::new(EchoShell {
        calls: Arc::clone(&calls),
    })))
    .await;
    host.shell(2, "echo hi");
    let (req_id, payload) = host.reply().await;
    assert_eq!(req_id, RequestId(2));
    assert!(matches!(payload, ResponsePayload::Ack), "got {payload:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the command ran once");
    let results = host.results().await;
    assert_eq!(results.len(), 1, "the log holds one result: {results:?}");
    assert_eq!(
        results[0]["stdout"],
        json!("ran echo hi"),
        "the result carries the command's output"
    );
}

/// The acknowledgement is the last frame of the command's reply, and the
/// result carrying the command's output arrives before it: the client renders
/// the call and its result before the reply clears the running state. An
/// acknowledgement sent first would drop the hint while the rows still stream
/// in.
#[tokio::test]
async fn test_ack_follows_transcript_frames() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut host = Host::start(Some(Arc::new(EchoShell { calls }))).await;
    host.shell(2, "echo hi");
    let frames = host.ordered_frames().await;
    let ack_at = frames
        .iter()
        .position(|frame| matches!(frame, ServerFrame::Response(_)))
        .expect("the command is acknowledged");
    assert_eq!(
        ack_at,
        frames.len() - 1,
        "the acknowledgement is the last frame: {frames:?}"
    );
    frames[..ack_at]
        .iter()
        .position(|frame| {
            tool_result_update(frame).is_some_and(|update| {
                update
                    .fields
                    .raw_output
                    .as_ref()
                    .is_some_and(|output| output["stdout"] == json!("ran echo hi"))
            })
        })
        .expect("the result carrying the command's output arrives before the acknowledgement");
}

/// A registry with no shell tool cannot run a typed command, and the refusal
/// names the missing tool instead of acknowledging work that never happened.
/// The category says the host cannot take the work now, a state that may
/// differ on a later attempt, not a defect in the command.
#[tokio::test]
async fn test_without_shell_tool_errors() {
    let mut host = Host::start(None).await;
    host.shell(2, "echo hi");
    let (req_id, payload) = host.reply().await;
    assert_eq!(req_id, RequestId(2));
    match payload {
        ResponsePayload::Error(e) => {
            assert_eq!(e.category, ErrorCategory::Unavailable);
            assert!(
                e.retriable,
                "a refusal the user can leave is marked retryable"
            );
            assert!(
                e.message.contains("no bash tool is registered"),
                "the refusal names the missing tool: {}",
                e.message
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// A command with no text is refused as an invalid request: repeating the
/// same input can never succeed, so it is not marked retryable and does not
/// claim the host is unavailable.
#[tokio::test]
async fn test_blank_command_invalid() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut host = Host::start(Some(Arc::new(EchoShell { calls }))).await;
    host.shell(2, "   ");
    let (req_id, payload) = host.reply().await;
    assert_eq!(req_id, RequestId(2));
    match payload {
        ResponsePayload::Error(e) => {
            assert_eq!(e.category, ErrorCategory::InvalidRequest);
            assert!(!e.retriable, "the same input cannot succeed on a retry");
            assert!(
                e.message.contains("empty"),
                "the refusal names the blank text: {}",
                e.message
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// A cancel issued while the command runs reaches the host's loop and stops
/// it: the acknowledgement to the cancel arrives before the command's own
/// reply, and the log holds one interrupted result.
#[tokio::test]
async fn test_cancel_stops_running_command() {
    let (started_tx, started_rx) = oneshot::channel();
    let mut host = Host::start(Some(Arc::new(HangingShell {
        started: Mutex::new(Some(started_tx)),
    })))
    .await;
    host.shell(2, "sleep 60");
    started_rx.await.expect("the command starts");
    host.cancel(3);
    let (first, first_payload) = host.reply().await;
    assert_eq!(first, RequestId(3), "the cancel is answered while it runs");
    assert!(matches!(first_payload, ResponsePayload::Ack));
    let (second, second_payload) = host.reply().await;
    assert_eq!(second, RequestId(2), "the command's own reply follows");
    assert!(matches!(second_payload, ResponsePayload::Ack));
    let results = host.results().await;
    assert_eq!(results.len(), 1, "the cancelled call keeps a result");
    assert_eq!(
        results[0]["not_run"],
        json!(true),
        "the result reports the interruption: {results:?}"
    );
}
