use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use houyicoder_async::PFut;
use houyicoder_client::{Client, Transport};
use houyicoder_protocol::envelope::{
    ClientFrame, EventEnvelope, EventSeq, ResponseEnvelope, ResponsePayload, ServerFrame,
    ServerRequestEnvelope, ServerRequestPayload,
};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::framing;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::event::FrontendEvent;
use houyicoder_protocol::handshake::Hello;

use super::*;
use crate::test_harness::connected_app_events;

fn sid() -> houyicoder_protocol::frontend::SessionId {
    houyicoder_protocol::frontend::SessionId::new("s1")
}

/// The inject notification's method + params must match exactly what the
/// server's handle_session_notification reads, else mid-turn injection
/// silently no-ops.
#[test]
fn test_inject_shape() {
    let input = houyicoder_protocol::frontend::QueuedInput::new("also check the logs");
    let n = inject_notification(&sid(), &input);
    assert_eq!(n.method, "session/inject");
    let p = n.params.expect("params present");
    assert_eq!(p.get("input"), Some(&serde_json::json!(input)));
    assert_eq!(p.get("sessionId").and_then(|v| v.as_str()), Some("s1"));
}

/// The queue_remove notification carries the exact queue identity.
#[test]
fn test_remove_shape() {
    let id = houyicoder_protocol::frontend::PendingInputId(42);
    let n = queue_remove_notification(&sid(), id);
    assert_eq!(n.method, "session/queue_remove");
    let p = n.params.expect("params present");
    assert_eq!(p.get("id"), Some(&serde_json::json!(id)));
}

/// The inject_child notification's method + params must match what the
/// server reads to route a steering text into a child's inbox, else
/// steering silently no-ops.
#[test]
fn test_inject_child_notif_shape() {
    let n = inject_child_notification("c1", "focus on the auth module");
    assert_eq!(n.method, "session/inject_child");
    let p = n.params.expect("params present");
    assert_eq!(p.get("childSid").and_then(|v| v.as_str()), Some("c1"));
    assert_eq!(
        p.get("text").and_then(|v| v.as_str()),
        Some("focus on the auth module")
    );
}

/// The abort-child-turn notification carries the childSid the server's
/// handle_session_notification reads. A typo in the method name or the
/// param key would make the per-turn abort silently no-op.
#[test]
fn test_cancel_child_notif_shape() {
    let n = cancel_child_turn_notification("c1");
    assert_eq!(n.method, "session/cancel_child_turn");
    let p = n.params.expect("params present");
    assert_eq!(p.get("childSid").and_then(|v| v.as_str()), Some("c1"));
}

/// The kill-all notification carries the method name the server's
/// handle_session_notification routes to kill_all_children. A typo would
/// make the fleet kill-all silently no-op.
#[test]
fn test_kill_all_notif_shape() {
    let n = kill_all_notification();
    assert_eq!(n.method, "session/kill_all");
}

/// The kill-child notification carries the method + childSid the server's
/// handle_session_notification routes to kill_child. A typo in either would
/// make a single-kill silently no-op.
#[test]
fn test_kill_child_notif_shape() {
    let n = kill_child_notification("c1");
    assert_eq!(n.method, "session/kill_child");
    let p = n.params.expect("params present");
    assert_eq!(p.get("childSid").and_then(|v| v.as_str()), Some("c1"));
}

// --- request-identifier allocation ---

/// A fresh connection issues its first identifier at zero and counts up by
/// one per call.
#[test]
fn test_request_id_starts_zero() {
    let (app, _events) = connected_app_events();
    let session = app.session.as_ref().expect("connected session");
    assert_eq!(session.next_request_id().unwrap(), RequestId(0));
    assert_eq!(session.next_request_id().unwrap(), RequestId(1));
}

/// The u64 boundary refuses allocation rather than wrapping onto zero, and
/// the refusal is stable: the counter stays at the maximum and every later
/// call keeps returning Exhausted.
#[test]
fn test_request_id_exhaustion_stable() {
    let (app, _events) = connected_app_events();
    let session = app.session.as_ref().expect("connected session");
    session.next_req_id.set(u64::MAX - 1);
    assert_eq!(
        session.next_request_id().unwrap(),
        RequestId(u64::MAX - 1),
        "the last identifier is issued at the boundary"
    );
    assert_eq!(session.next_request_id(), Err(RequestIdExhausted));
    assert_eq!(session.next_request_id(), Err(RequestIdExhausted));
    assert_eq!(
        session.next_req_id.get(),
        u64::MAX,
        "the counter never wraps"
    );
    assert!(
        session.take_exhaustion_notice(),
        "the first exhaustion observation announces once"
    );
    assert!(
        !session.take_exhaustion_notice(),
        "later observations stay quiet instead of repeating"
    );
    assert_eq!(
        format!("{}", RequestIdExhausted),
        "request ID sequence exhausted",
        "the error carries a readable message"
    );
}

/// Each connection owns an independent sequence: advancing one leaves a
/// fresh connection still issuing from zero.
#[test]
fn test_request_id_sequence_independent() {
    let (a, _ea) = connected_app_events();
    let (b, _eb) = connected_app_events();
    let first = a.session.as_ref().expect("connected session");
    let second = b.session.as_ref().expect("connected session");
    first.next_req_id.set(7);
    assert_eq!(first.next_request_id().unwrap(), RequestId(7));
    assert_eq!(second.next_request_id().unwrap(), RequestId(0));
}

/// A KillChild command drains through the driver as a session/kill_child
/// notification on the wire. Pins the driver dispatch mapping the pure
/// shape test cannot reach.
#[tokio::test]
async fn test_drive_kill_child_forwards() {
    let run = FakeEngine::new()
        .drive(vec![ClientCommand::KillChild {
            child_sid: "c1".into(),
        }])
        .await;
    assert!(
        run.sent
            .iter()
            .any(|f| f.contains("session/kill_child") && f.contains("c1")),
        "the KillChild command forwarded a session/kill_child notification: {run:?}"
    );
}

/// A CancelChildTurn command drains through the driver as a
/// session/cancel_child_turn notification on the wire. Pins the driver
/// dispatch for the per-turn interrupt path that Esc fires when viewing a
/// running child. Mirrors the kill-child forwarding test.
#[tokio::test]
async fn test_drive_cancel_child_forwards() {
    let run = FakeEngine::new()
        .drive(vec![ClientCommand::CancelChildTurn {
            child_sid: "c1".into(),
        }])
        .await;
    assert!(
        run.sent
            .iter()
            .any(|f| f.contains("session/cancel_child_turn") && f.contains("c1")),
        "the CancelChildTurn command forwarded a session/cancel_child_turn notification: {run:?}"
    );
}

/// A read failure (the engine closed or the transport broke mid-stream)
/// must surface as ConnectionLost so the App clears agent_busy and sweeps
/// pending pane marks. The prior silent return wedged the TUI on any
/// engine-side fatal.
#[tokio::test]
async fn test_drive_client_read_done() {
    let mut engine = FakeEngine::new();
    engine.close();
    let run = engine.drive(Vec::new()).await;
    match run.msgs.last() {
        Some(AgentMessage::ConnectionLost { message }) => {
            assert!(
                message.contains("connection lost"),
                "expected a connection-lost message, got: {message}"
            );
        }
        other => panic!("expected ConnectionLost on read error, got {other:?}"),
    }
}

/// A RenameSessionQuery forwards with the req_id and both identity fields,
/// so the status command's rename lands on the intended session.
#[tokio::test]
async fn test_drive_forwards_rename_command() {
    use houyicoder_protocol::frontend::SessionId;

    let run = FakeEngine::new()
        .drive(vec![ClientCommand::RenameSessionQuery {
            req_id: RequestId(1),
            session_id: SessionId::new("s1"),
            name: "daily-fixes".into(),
        }])
        .await;
    assert!(
        run.has_request(
            1,
            |p| matches!(p, FrontendRequest::RenameSession { session_id, name }
                if session_id.0 == "s1" && name == "daily-fixes")
        ),
        "rename lost its session identity in dispatch: {run:?}"
    );
    assert_eq!(run.requests().len(), 1);
}

/// A ServerFrame::Event carrying AgentStatus is translated to
/// AgentMessage::AgentStatus by the driver, preserving every field.
#[tokio::test]
async fn test_drive_translates_agent_status() {
    let mut engine = FakeEngine::new();
    engine.event(FrontendEvent::AgentStatus {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        turn: 2,
        tokens: 150,
        tool_uses: 3,
        last_activity: Some("grep".into()),
        completed: None,
    });
    engine.close();
    let run = engine.drive(Vec::new()).await;
    match run.msgs.first() {
        Some(AgentMessage::AgentStatus {
            agent_id,
            turn,
            tokens,
            ..
        }) => {
            assert_eq!(agent_id, "c1");
            assert_eq!(*turn, 2);
            assert_eq!(*tokens, 150);
        }
        other => panic!("expected AgentStatus, got {other:?}"),
    }
}

// --- driver dispatch and translation contracts ---

/// A fake engine at the other end of the connection. It speaks the full
/// handshake, plays the frames it was loaded with in order (an exhausted
/// load blocks on reads), records every frame the client sent, and can
/// break its write side after a counted send to exercise send-failure
/// deaths.
struct FakeEngine {
    seq: u64,
    next_req: u64,
    sent_count: usize,
    fail_sends_after: Option<usize>,
    load: VecDeque<Result<String, ProtocolError>>,
    sent: Arc<Mutex<Vec<String>>>,
}

impl FakeEngine {
    /// An engine that completes the handshake and then waits.
    fn new() -> Self {
        Self {
            seq: 0,
            next_req: 0,
            sent_count: 0,
            fail_sends_after: None,
            load: VecDeque::from(vec![Ok(line_of(&Hello::local()))]),
            sent: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Break the send side after n successful sends (1 = right after the
    /// handshake).
    fn fail_sends_after(mut self, n: usize) -> Self {
        self.fail_sends_after = Some(n);
        self
    }

    /// Queue an event for the client, issuing the next event seq.
    fn event(&mut self, e: FrontendEvent) {
        self.seq += 1;
        self.load
            .push_back(Ok(line_of(&ServerFrame::Event(EventEnvelope::new(
                EventSeq(self.seq),
                e,
            )))));
    }

    /// Queue a response for the client, issuing the next request id.
    fn response(&mut self, payload: ResponsePayload) {
        self.next_req += 1;
        self.load
            .push_back(Ok(line_of(&ServerFrame::Response(ResponseEnvelope::new(
                RequestId(self.next_req),
                payload,
            )))));
    }

    /// Queue a reverse request the client must answer.
    fn reverse_request(&mut self, req_id: u64, payload: ServerRequestPayload) {
        self.load.push_back(Ok(line_of(&ServerFrame::Request(
            ServerRequestEnvelope::new(RequestId(req_id), payload),
        ))));
    }

    /// Break the read side: the engine ends the connection once its queued
    /// frames are delivered.
    fn close(&mut self) {
        self.load.push_back(Err(ProtocolError::new(
            ErrorCategory::Unavailable,
            "done",
            false,
        )));
    }

    /// Run the connection against this engine with the commands preloaded
    /// and collect what the driver did. The command channel stays open so
    /// the driver lives until the engine ends the connection; a cap bounds
    /// the wait when it never does.
    async fn drive(self, commands: Vec<ClientCommand>) -> SessionRun {
        let client = Client::new(Box::new(self));
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<ClientCommand>();
        let (agent_tx, agent_rx) = std::sync::mpsc::channel::<AgentMessage>();
        for cmd in commands {
            cmd_tx.send(cmd).ok();
        }
        let _keep_alive = cmd_tx;
        let _ = tokio::time::timeout(
            Duration::from_millis(300),
            drive_client(client, cmd_rx, agent_tx),
        )
        .await;
        let mut msgs = Vec::new();
        while let Ok(m) = agent_rx.try_recv() {
            msgs.push(m);
        }
        let sent = SENT_LOG.with(|c| c.borrow_mut().take());
        SessionRun {
            msgs,
            sent: sent
                .map(|log| log.lock().unwrap().clone())
                .unwrap_or_default(),
        }
    }
}

thread_local! {
    /// The engine records sends into a thread-local log so the Transport
    /// impl can stay on an owned struct; drive() takes it back out at the
    /// end of the run.
    static SENT_LOG: RefCell<Option<Arc<Mutex<Vec<String>>>>> = const { RefCell::new(None) };
}

impl Transport for FakeEngine {
    fn send_frame(&mut self, frame: &str) -> PFut<'_, Result<(), ProtocolError>> {
        self.sent_count += 1;
        if self.fail_sends_after.is_some_and(|n| self.sent_count > n) {
            return Box::pin(async {
                Err(ProtocolError::new(
                    ErrorCategory::Unavailable,
                    "pipe broken",
                    false,
                ))
            });
        }
        self.sent.lock().unwrap().push(frame.to_string());
        Box::pin(async { Ok(()) })
    }

    fn recv_frame(&mut self) -> PFut<'_, Result<Option<String>, ProtocolError>> {
        match self.load.pop_front() {
            Some(Ok(line)) => Box::pin(async move { Ok(Some(line)) }),
            Some(Err(e)) => Box::pin(async move { Err(e) }),
            None => Box::pin(async {
                std::future::pending::<()>().await;
                Ok(None)
            }),
        }
    }
}

impl Drop for FakeEngine {
    fn drop(&mut self) {
        SENT_LOG.with(|c| {
            let mut slot = c.borrow_mut();
            if slot.is_none() {
                *slot = Some(self.sent.clone());
            }
        });
    }
}

/// What one connection against a fake engine produced: every message the
/// driver emitted and every frame it sent.
#[derive(Debug)]
struct SessionRun {
    msgs: Vec<AgentMessage>,
    sent: Vec<String>,
}

impl SessionRun {
    /// Whether a request carrying this req_id and payload shape was sent.
    fn has_request(&self, id: u64, payload: fn(&FrontendRequest) -> bool) -> bool {
        self.requests().iter().any(|f| match f {
            ClientFrame::Request(env) => env.req_id == RequestId(id) && payload(&env.payload),
            _ => false,
        })
    }

    /// The requests the driver sent, decoded to typed envelopes; the
    /// handshake and the notifications are not requests.
    fn requests(&self) -> Vec<ClientFrame> {
        self.sent
            .iter()
            .filter_map(|line| serde_json::from_str::<ClientFrame>(line.trim_end()).ok())
            .filter(|f| matches!(f, ClientFrame::Request(_)))
            .collect()
    }
}

/// Encode one value as a framed line the transport layer can deliver.
fn line_of<T: serde::Serialize>(value: &T) -> String {
    framing::encode(value).expect("encode")
}

/// Each query command the driver forwards must carry the minted req_id and
/// the exact request payload; a mismatched dispatch silently drops the verb
/// and the reply never routes.
#[tokio::test]
async fn test_drive_forwards_query_commands() {
    let run = FakeEngine::new()
        .drive(vec![
            ClientCommand::TrajectoryQuery {
                req_id: RequestId(1),
            },
            ClientCommand::ToolListQuery {
                req_id: RequestId(2),
            },
            ClientCommand::HooksQuery {
                req_id: RequestId(3),
            },
            ClientCommand::MemoryShowQuery {
                req_id: RequestId(4),
                key: "key-a".into(),
            },
            ClientCommand::PermissionAskBeforeGitQuery {
                req_id: RequestId(5),
                enabled: None,
            },
            ClientCommand::PermissionAskBeforeGitQuery {
                req_id: RequestId(6),
                enabled: Some(true),
            },
        ])
        .await;
    assert!(run.has_request(1, |p| matches!(p, FrontendRequest::Trajectory)));
    assert!(run.has_request(2, |p| matches!(p, FrontendRequest::ToolList)));
    assert!(run.has_request(3, |p| matches!(p, FrontendRequest::Hooks)));
    assert!(
        run.has_request(
            4,
            |p| matches!(p, FrontendRequest::MemoryShow { key } if key == "key-a")
        ),
        "MemoryShow key lost in dispatch: {run:?}"
    );
    assert!(
        run.has_request(5, |p| matches!(
            p,
            FrontendRequest::PermissionAskBeforeGit { enabled: None }
        )),
        "the None form (a read) must forward unchanged: {run:?}"
    );
    assert!(
        run.has_request(6, |p| matches!(
            p,
            FrontendRequest::PermissionAskBeforeGit {
                enabled: Some(true)
            }
        )),
        "the Some form (a write) must forward unchanged: {run:?}"
    );
    assert_eq!(
        run.requests().len(),
        6,
        "exactly the six query commands: {run:?}"
    );
}

/// KillAllChildren rides as a notification; SessionReset and DebugSet carry
/// their identity fields through the dispatch.
#[tokio::test]
async fn test_drive_forwards_admin_commands() {
    use houyicoder_protocol::frontend::SessionId;
    use houyicoder_protocol::frontend::debug::DebugLevel;

    let run = FakeEngine::new()
        .drive(vec![
            ClientCommand::KillAllChildren,
            ClientCommand::SessionReset {
                req_id: RequestId(1),
                session_id: SessionId::new("s1"),
            },
            ClientCommand::DebugSet {
                req_id: RequestId(2),
                level: DebugLevel::Debug,
            },
        ])
        .await;
    assert!(
        run.sent.iter().any(|f| f.contains("session/kill_all")),
        "KillAllChildren forwarded a session/kill_all notification: {run:?}"
    );
    assert!(
        run.has_request(
            1,
            |p| matches!(p, FrontendRequest::SessionReset { session_id } if session_id.0 == "s1")
        ),
        "SessionReset sid lost in dispatch: {run:?}"
    );
    assert!(
        run.has_request(2, |p| matches!(
            p,
            FrontendRequest::DebugSet {
                level: DebugLevel::Debug
            }
        )),
        "DebugSet level lost in dispatch: {run:?}"
    );
    assert_eq!(
        run.requests().len(),
        2,
        "exactly the two admin requests: {run:?}"
    );
}

/// The remaining permission and skill queries forward with their identity
/// fields: the rules list, the skills list, and a working-dir removal that
/// must carry the exact path.
#[tokio::test]
async fn test_drive_forwards_config_queries() {
    use houyicoder_protocol::frontend::FrontendRequest;

    let run = FakeEngine::new()
        .drive(vec![
            ClientCommand::PermissionRulesQuery {
                req_id: RequestId(1),
            },
            ClientCommand::SkillsQuery {
                req_id: RequestId(2),
            },
            ClientCommand::PermissionRemoveDirQuery {
                req_id: RequestId(3),
                path: "/work".into(),
            },
        ])
        .await;
    assert!(
        run.has_request(1, |p| matches!(p, FrontendRequest::PermissionRules)),
        "rules query lost in dispatch: {run:?}"
    );
    assert!(
        run.has_request(2, |p| matches!(p, FrontendRequest::Skills)),
        "skills query lost in dispatch: {run:?}"
    );
    assert!(
        run.has_request(
            3,
            |p| matches!(p, FrontendRequest::PermissionRemoveWorkingDir { path } if path == "/work")
        ),
        "dir removal lost its path in dispatch: {run:?}"
    );
    assert_eq!(
        run.requests().len(),
        3,
        "exactly the three config queries: {run:?}"
    );
}

/// Modeled events translate with identity intact; an event kind the driver
/// does not model is skipped without ending the stream (the frames after it
/// still arrive).
#[tokio::test]
async fn test_drive_translates_events() {
    use houyicoder_protocol::frontend::QueuedInput;
    use houyicoder_protocol::frontend::memory::{
        MemoryChange, MemoryChangeId, MemoryChangeOrigin, MemoryOperation,
    };

    let mut engine = FakeEngine::new();
    engine.event(FrontendEvent::QueuedInputCommitted {
        inputs: vec![QueuedInput::new("queued text")],
    });
    engine.event(FrontendEvent::MemoryChanged {
        id: MemoryChangeId("m1".into()),
        origin: MemoryChangeOrigin::AutoMemory,
        changes: vec![MemoryChange {
            key: "topic".into(),
            operation: MemoryOperation::Stored,
        }],
    });
    engine.event(FrontendEvent::SystemLine {
        text: "notice".into(),
    });
    engine.event(FrontendEvent::Metrics {
        tokens: 10,
        cache_hit_ratio: 0.5,
    });
    engine.close();
    let run = engine.drive(Vec::new()).await;

    assert!(
        run.msgs.iter().any(
            |m| matches!(m, AgentMessage::QueuedInputCommitted { inputs }
                if inputs.len() == 1 && inputs[0].text == "queued text")
        ),
        "queue commit identity lost: {run:?}"
    );
    assert!(
        run.msgs.iter().any(
            |m| matches!(m, AgentMessage::MemoryChanged { id, changes, .. }
                if id.0 == "m1" && changes.len() == 1)
        ),
        "memory change identity lost: {run:?}"
    );
    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::SystemLine { text } if text == "notice")),
        "system line text lost: {run:?}"
    );
    assert!(
        matches!(run.msgs.last(), Some(AgentMessage::ConnectionLost { .. })),
        "the terminal read error ends the driver: {run:?}"
    );
    // the Metrics event is skipped, not translated and not fatal
    assert_eq!(
        run.msgs.len(),
        3 + 1,
        "three events plus the death: {run:?}"
    );
}

/// Tool progress needs both the call id and the elapsed seconds; a missing
/// one drops the tick, and the optional line count rides along only when
/// the engine sent it.
#[tokio::test]
async fn test_drive_translates_tool_progress() {
    use houyicoder_protocol::acpx::{AcpxMethod, AcpxNotification};

    let mut engine = FakeEngine::new();
    engine.event(FrontendEvent::Acpx {
        notification: AcpxNotification::new(
            AcpxMethod::ToolProgress,
            serde_json::json!({"call_id": "call-7", "elapsed_secs": 4, "lines": 12}),
        ),
    });
    engine.event(FrontendEvent::Acpx {
        notification: AcpxNotification::new(
            AcpxMethod::ToolProgress,
            serde_json::json!({"call_id": "call-8", "elapsed_secs": 2}),
        ),
    });
    engine.event(FrontendEvent::Acpx {
        notification: AcpxNotification::new(
            AcpxMethod::ToolProgress,
            serde_json::json!({"call_id": "call-9"}),
        ),
    });
    engine.close();
    let run = engine.drive(Vec::new()).await;

    assert!(
        run.msgs.iter().any(
            |m| matches!(m, AgentMessage::ToolProgress { call_id, elapsed_secs, lines: Some(12) }
                if call_id == "call-7" && *elapsed_secs == 4)
        ),
        "full progress tick lost its fields: {run:?}"
    );
    assert!(
        run.msgs.iter().any(
            |m| matches!(m, AgentMessage::ToolProgress { call_id, lines: None, .. }
                if call_id == "call-8")
        ),
        "absent line count must surface as None: {run:?}"
    );
    assert!(
        !run.msgs.iter().any(
            |m| matches!(m, AgentMessage::ToolProgress { call_id, .. } if call_id == "call-9")
        ),
        "a tick without elapsed_secs must be dropped, not half-built: {run:?}"
    );
    assert_eq!(run.msgs.len(), 2 + 1, "two ticks plus the death: {run:?}");
}

/// Core responses translate with their fields intact; Ack maps to no
/// message at all.
#[tokio::test]
async fn test_drive_translates_core_responses() {
    use houyicoder_protocol::frontend::compact::CompactReply;
    use houyicoder_protocol::frontend::context::ContextBreakdown;
    use houyicoder_protocol::frontend::run::RunError;
    use houyicoder_protocol::frontend::status::StatusSnapshot;
    use houyicoder_protocol::frontend::trajectory::TrajectoryResponse;

    let mut engine = FakeEngine::new();
    engine.response(ResponsePayload::Ack);
    engine.response(ResponsePayload::Status(StatusSnapshot::default()));
    engine.response(ResponsePayload::RunErr(RunError {
        category: "provider".into(),
        message: "exhausted".into(),
    }));
    engine.response(ResponsePayload::Error(ProtocolError::new(
        ErrorCategory::InvalidRequest,
        "bad verb",
        false,
    )));
    engine.response(ResponsePayload::Trajectory(TrajectoryResponse {
        entries: Vec::new(),
        redundant: Vec::new(),
        unknown_count: 0,
    }));
    engine.response(ResponsePayload::Context(ContextBreakdown::default()));
    engine.response(ResponsePayload::Compact(CompactReply::new(
        false, 0, "m0", None, None,
    )));
    engine.close();
    let run = engine.drive(Vec::new()).await;

    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::Done { result: Err(e) }
                if e.category == "provider" && e.message == "exhausted")),
        "run error must surface as Done with both fields: {run:?}"
    );
    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::RequestError { message, .. }
                if message.contains("bad verb"))),
        "protocol error must carry the message: {run:?}"
    );
    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::StatusResult { .. }))
    );
    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::TrajectoryResult { .. }))
    );
    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::ContextResult { .. }))
    );
    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::CompactResult { .. }))
    );
    assert_eq!(
        run.msgs.len(),
        6 + 1,
        "Ack maps to nothing, the other six translate, plus the death: {run:?}"
    );
}

/// Permission and debug answers translate with their identity fields.
#[tokio::test]
async fn test_drive_translates_permission_responses() {
    use houyicoder_protocol::frontend::debug::DebugState;
    use houyicoder_protocol::frontend::permission::{PermissionMode, PermissionRule};

    let mut engine = FakeEngine::new();
    engine.response(ResponsePayload::PermissionMode(PermissionMode::Auto));
    engine.response(ResponsePayload::PermissionRules(vec![
        PermissionRule::default(),
    ]));
    engine.response(ResponsePayload::PermissionWorkingDirs(vec!["/work".into()]));
    engine.response(ResponsePayload::PermissionAskBeforeGit(true));
    engine.response(ResponsePayload::Debug(DebugState {
        enabled: true,
        path: "/tmp/houyi.log".into(),
    }));
    engine.close();
    let run = engine.drive(Vec::new()).await;

    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::PermissionModeResult { .. }))
    );
    assert!(
        run.msgs.iter().any(
            |m| matches!(m, AgentMessage::PermissionRulesResult { rules } if rules.len() == 1)
        )
    );
    assert!(run.msgs.iter().any(
        |m| matches!(m, AgentMessage::PermissionDirsResult { dirs, .. }
            if dirs.as_slice() == ["/work"])
    ));
    assert!(run.msgs.iter().any(|m| matches!(
        m,
        AgentMessage::PermissionAskBeforeGitResult { enabled: true }
    )));
    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::DebugResult { state }
            if state.enabled && state.path == "/tmp/houyi.log"))
    );
    assert_eq!(
        run.msgs.len(),
        5 + 1,
        "five answers plus the death: {run:?}"
    );
}

/// Tool, agent, child, hook, and skill listings translate with their
/// entries intact.
#[tokio::test]
async fn test_drive_translates_catalog_responses() {
    use houyicoder_protocol::frontend::SessionId;
    use houyicoder_protocol::frontend::hooks::HookEntry;
    use houyicoder_protocol::frontend::skills::SkillEntry;
    use houyicoder_protocol::frontend::tools::ToolEntry;

    let mut engine = FakeEngine::new();
    engine.response(ResponsePayload::Tools(vec![ToolEntry {
        name: "grep".into(),
        description: "search files".into(),
    }]));
    engine.response(ResponsePayload::Agents("explore".into()));
    engine.response(ResponsePayload::ChildTranscript {
        child_sid: SessionId::new("c1"),
        frames: Vec::new(),
    });
    engine.response(ResponsePayload::Hooks(vec![HookEntry {
        name: "guard".into(),
        events: vec!["pre_tool".into()],
        source: "project".into(),
        fired: false,
        summary: String::new(),
        description: String::new(),
    }]));
    engine.response(ResponsePayload::Skills(vec![SkillEntry {
        name: "deploy".into(),
        description: "ship it".into(),
        origin: "user".into(),
        invocable: true,
        user_invocable: false,
        body_token_estimate: 12,
        usage: None,
    }]));
    engine.close();
    let run = engine.drive(Vec::new()).await;

    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolListResult { tools }
            if tools.len() == 1 && tools[0].name == "grep"))
    );
    assert!(
        run.msgs.iter().any(
            |m| matches!(m, AgentMessage::AgentsResult { directory } if directory == "explore")
        )
    );
    assert!(run.msgs.iter().any(
        |m| matches!(m, AgentMessage::ChildTranscriptResult { child_sid, frames }
            if child_sid == "c1" && frames.is_empty())
    ));
    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::HooksResult { hooks }
            if hooks.len() == 1 && hooks[0].name == "guard"))
    );
    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::SkillsResult { skills }
            if skills.len() == 1 && skills[0].name == "deploy"))
    );
    assert_eq!(
        run.msgs.len(),
        5 + 1,
        "five listings plus the death: {run:?}"
    );
}

/// Memory and undo answers translate; an absent memory key and an empty
/// undo stack come back as the None half of the same message, and the reply
/// keeps the req_id its request was minted with.
#[tokio::test]
async fn test_drive_translates_memory_responses() {
    use houyicoder_protocol::frontend::memory::{MemoryDetail, MemorySummaryEntry, ToggleState};

    let mut engine = FakeEngine::new();
    engine.response(ResponsePayload::MemoryList(vec![MemorySummaryEntry {
        key: "topic".into(),
        description: "the topic".into(),
        source: "user".into(),
        scope: "project".into(),
        mtime_secs: 5,
    }]));
    engine.response(ResponsePayload::MemoryShow(Some(MemoryDetail {
        key: "topic".into(),
        content: "body".into(),
        source: "user".into(),
        description: "the topic".into(),
        mtime_secs: 5,
    })));
    engine.response(ResponsePayload::MemoryShow(None));
    engine.response(ResponsePayload::ToggleState(ToggleState {
        auto_memory: true,
        auto_dream: false,
    }));
    engine.response(ResponsePayload::UndoResult(None));
    engine.response(ResponsePayload::UndoResult(Some("undid the edit".into())));
    engine.close();
    let run = engine.drive(Vec::new()).await;

    assert!(
        run.msgs.iter().any(
            |m| matches!(m, AgentMessage::MemoryListResult { req_id, entries }
                if *req_id == RequestId(1) && entries.len() == 1)
        ),
        "memory list keeps its request identity: {run:?}"
    );
    assert!(
        run.msgs.iter().any(
            |m| matches!(m, AgentMessage::MemoryShowResult { entry: Some(d), .. }
                if d.key == "topic")
        ),
        "memory show carries the body: {run:?}"
    );
    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::MemoryShowResult { entry: None, .. })),
        "absent memory key must surface as None, not vanish: {run:?}"
    );
    assert!(run.msgs.iter().any(
        |m| matches!(m, AgentMessage::MemoryToggleStateResult { state, .. }
            if state.auto_memory && !state.auto_dream)
    ));
    assert!(
        run.msgs.iter().any(
            |m| matches!(m, AgentMessage::UndoResult { description: Some(d) } if d == "undid the edit")
        ),
        "undo description lost: {run:?}"
    );
    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::UndoResult { description: None })),
        "empty undo stack must surface as None: {run:?}"
    );
    assert_eq!(run.msgs.len(), 6 + 1, "six answers plus the death: {run:?}");
}

/// Model answers translate, and the server's reverse trust request keeps
/// its own req_id so the reply can route back.
#[tokio::test]
async fn test_drive_translates_trust_ask() {
    use houyicoder_protocol::envelope::ModelApplied;
    use houyicoder_protocol::frontend::model::ModelCatalog;
    use houyicoder_protocol::frontend::trust::TrustPrompt;

    let mut engine = FakeEngine::new();
    engine.response(ResponsePayload::ModelResult(ModelApplied {
        model: "qwen3.7-max".into(),
        effort: None,
    }));
    engine.response(ResponsePayload::ModelInfo(ModelCatalog::default()));
    engine.reverse_request(
        90,
        ServerRequestPayload::TrustPrompt(TrustPrompt {
            project_path: "/proj".into(),
            risks: Vec::new(),
        }),
    );
    engine.close();
    let run = engine.drive(Vec::new()).await;

    assert!(run.msgs.iter().any(
        |m| matches!(m, AgentMessage::ModelResult { model, effort: None }
            if model == "qwen3.7-max")
    ));
    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::ModelInfoResult { .. }))
    );
    assert!(
        run.msgs
            .iter()
            .any(|m| matches!(m, AgentMessage::TrustAsk { req_id, prompt }
                if *req_id == RequestId(90) && prompt.project_path == "/proj")),
        "trust ask must carry the reverse req_id: {run:?}"
    );
    assert_eq!(
        run.msgs.len(),
        2 + 1 + 1,
        "two model answers, the trust ask, the death: {run:?}"
    );
}

/// A send failure after the handshake kills the driver with a send-failed
/// death message; swallowing it would leave the TUI waiting on a reply that
/// can never arrive.
#[tokio::test]
async fn test_send_failure_announces_death() {
    let run = FakeEngine::new()
        .fail_sends_after(1)
        .drive(vec![ClientCommand::StatusQuery {
            req_id: RequestId(1),
        }])
        .await;
    match run.msgs.last() {
        Some(AgentMessage::ConnectionLost { message }) => {
            assert!(
                message.contains("send failed"),
                "expected a send-failed death, got: {message}"
            );
        }
        other => panic!("expected ConnectionLost, got {other:?}"),
    }
}
