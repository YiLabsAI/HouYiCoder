//! Tests for session lifecycle and transcript folding.

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::future::pending;
use std::io::Cursor;
use std::io::Write;
use std::io::sink;
use std::process::id;
use std::sync::{Arc, Mutex};

use super::super::transcript::TranscriptFold;
use super::*;
use crate::acp::pump::from_line_channels;
use houyicoder_api::launcher::{
    LauncherChild, LauncherExit, ProcessLauncher, SpawnError, SpawnPolicy, SpawnRequest, StdioPipes,
};
use houyicoder_api::session::SessionLog;
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::{DefaultModeGate, ModeGate, PermissionMode};
use houyicoder_session::SessionStore;
use tokio::sync::mpsc;

/// The grace window these tests allow: long enough for a channel round trip,
/// far below the wall-clock a production grace would add to the suite.
const TEST_GRACE: Duration = Duration::from_millis(15);

fn channel_session() -> (
    AcpChildSession,
    mpsc::Sender<String>,
    mpsc::Receiver<String>,
) {
    let (client_tx, server_rx) = mpsc::channel::<String>(16);
    let (server_tx, client_rx) = mpsc::channel::<String>(16);
    let pump = from_line_channels(server_rx, server_tx);
    (
        AcpChildSession::from_pump(pump).with_graces(TEST_GRACE, TEST_GRACE),
        client_tx,
        client_rx,
    )
}

/// Read outbound frames until one contains the marker, proving the task
/// reached that point. A latch instead of a fixed sleep.
async fn await_outbound(rx: &mut mpsc::Receiver<String>, marker: &str) {
    loop {
        let frame = rx.recv().await.expect("outbound frame arrived");
        if frame.contains(marker) {
            return;
        }
    }
}

#[test]
fn test_construction_error_shows_program() {
    let err = AcpClientError::ConstructionFailed {
        program: "devin".into(),
    };
    assert!(format!("{err}").contains("devin"));
}

#[test]
fn test_handshake_timeout_display() {
    let err = AcpClientError::HandshakeTimedOut;
    assert!(format!("{err}").contains("timed out"));
}

#[test]
fn test_child_exited_display() {
    let err = AcpClientError::ChildExited("pipe closed".into());
    assert!(format!("{err}").contains("pipe closed"));
}

#[tokio::test]
async fn test_next_request_id_increments() {
    let (mut session, _tx, _rx) = channel_session();
    let a = session.next_request_id();
    let b = session.next_request_id();
    assert_eq!(b, a + 1);
}

#[tokio::test]
async fn test_await_response_delivers() {
    let (mut session, tx, _rx) = channel_session();
    let resp = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
    let id = session.next_request_id();
    let task = tokio::spawn(async move {
        let val = session.await_response(id).await.unwrap();
        assert_eq!(val["ok"], serde_json::json!(true));
    });
    tx.send(format!("{resp}\n")).await.unwrap();
    task.await.unwrap();
}

/// A response that arrives after the reader has closed is still delivered
/// through the waiter, because the channel end does not drop a response the
/// waiter table already holds.
#[tokio::test]
async fn test_await_response_after_eof() {
    let (mut session, tx, _rx) = channel_session();
    let resp = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
    let id = session.next_request_id();
    let task = tokio::spawn(async move {
        let val = session.await_response(id).await.unwrap();
        assert_eq!(val["ok"], serde_json::json!(true));
    });
    // Deliver the response and close the channel, so the reader ends before
    // the waiter resolves.
    tx.send(format!("{resp}\n")).await.unwrap();
    drop(tx);
    task.await.unwrap();
}

/// A reader that ends before the response arrives fails the wait instead of
/// hanging, because no later frame can answer it.
#[tokio::test]
async fn test_await_response_eof_errors() {
    let (mut session, tx, _rx) = channel_session();
    let id = session.next_request_id();
    drop(tx);
    let err = session
        .await_response(id)
        .await
        .expect_err("a closed reader cannot answer");
    assert!(format!("{err}").contains("closed"), "{err}");
}

/// Banner lines before the first response are skipped during handshake.
#[tokio::test]
async fn test_banner_skipped_during_handshake() {
    let (mut session, tx, mut rx) = channel_session();
    let workspace = temp_dir();
    let task = tokio::spawn(async move { session.handshake_inner(&workspace).await });
    tx.send("starting up...\n".into()).await.unwrap();
    tx.send("ready\n".into()).await.unwrap();
    let init_resp = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#;
    tx.send(format!("{init_resp}\n")).await.unwrap();
    // Wait for the task to send session/new before sending its response.
    await_outbound(&mut rx, "session/new").await;
    let new_resp = r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"01JTESTSESSIONID01"}}"#;
    tx.send(format!("{new_resp}\n")).await.unwrap();
    let sid = task.await.unwrap().unwrap();
    assert!(!sid.to_string().is_empty());
}

/// The handshake completes when initialize and session/new responses arrive.
#[tokio::test]
async fn test_handshake_completes() {
    let (mut session, tx, mut rx) = channel_session();
    let init_resp = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#;
    let new_resp = r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"01JTESTSESSIONID01"}}"#;
    let workspace = temp_dir();
    let task = tokio::spawn(async move {
        session
            .handshake_inner(&workspace)
            .await
            .expect("handshake completes")
    });
    tx.send(format!("{init_resp}\n")).await.unwrap();
    await_outbound(&mut rx, "session/new").await;
    tx.send(format!("{new_resp}\n")).await.unwrap();
    let sid = task.await.unwrap();
    assert!(!sid.to_string().is_empty());
}

/// A child session id in this host's own id space is adopted as is, so the
/// child's log lands under the id the child named.
#[tokio::test]
async fn test_handshake_adopts_child_id() {
    let (mut session, tx, mut rx) = channel_session();
    let known = SessionId::new().to_string();
    let init_resp = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#;
    let new_resp = format!(r#"{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{known}"}}}}"#);
    let workspace = temp_dir();
    let task = tokio::spawn(async move { session.handshake_inner(&workspace).await });
    tx.send(format!("{init_resp}\n")).await.unwrap();
    await_outbound(&mut rx, "session/new").await;
    tx.send(format!("{new_resp}\n")).await.unwrap();
    let sid = task.await.unwrap().expect("handshake completes");
    assert_eq!(sid.to_string(), known, "the child's own id is adopted");
}

/// A child session id in another spelling mints a local id rather than
/// decoupling the log from the child, and the mapping is logged.
#[tokio::test]
async fn test_handshake_mints_local_id() {
    let (mut session, tx, mut rx) = channel_session();
    let init_resp = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#;
    let new_resp = r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"codex-session-1"}}"#;
    let workspace = temp_dir();
    let task = tokio::spawn(async move { session.handshake_inner(&workspace).await });
    tx.send(format!("{init_resp}\n")).await.unwrap();
    await_outbound(&mut rx, "session/new").await;
    tx.send(format!("{new_resp}\n")).await.unwrap();
    let sid = task.await.unwrap().expect("handshake completes");
    assert_ne!(sid.to_string(), "codex-session-1");
    assert!(!sid.to_string().is_empty(), "a local id is minted");
}

/// The handshake timeout fires when the child does not respond in time.
#[tokio::test]
async fn test_handshake_times_out() {
    let (mut session, _tx, _rx) = channel_session();
    let spec = ExternalAcpSpec {
        program: "test".into(),
        args: Vec::new(),
        passthrough_env: Vec::new(),
        env: Vec::new(),
        handshake_timeout_ms: 50,
    };
    let workspace = temp_dir();
    let err = session
        .handshake(&spec, &workspace)
        .await
        .expect_err("must time out");
    assert!(matches!(err, AcpClientError::HandshakeTimedOut));
}

/// A malformed initialize response surfaces as a Protocol error.
#[tokio::test]
async fn test_handshake_bad_init() {
    let (mut session, tx, _rx) = channel_session();
    let bad_resp = r#"{"jsonrpc":"2.0","id":1,"result":"not an object"}"#;
    let workspace = temp_dir();
    let task = tokio::spawn(async move { session.handshake_inner(&workspace).await });
    tx.send(format!("{bad_resp}\n")).await.unwrap();
    let err = task.await.unwrap().unwrap_err();
    assert!(matches!(err, AcpClientError::Protocol(_)));
}

#[tokio::test]
async fn test_recv_returns_stop_reason() {
    let (mut session, tx, _rx) = channel_session();
    let prompt_id = session.next_request_id();
    let notif = r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}}"#;
    tx.send(format!("{notif}\n")).await.unwrap();
    let resp = format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{{"stopReason":"end_turn"}}}}"#,
        prompt_id
    );
    tx.send(format!("{resp}\n")).await.unwrap();
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let sid = SessionId::new();
    let mut fold = TranscriptFold::new(sid, store.clone());
    let stop = session
        .recv_until_prompt(prompt_id, &mut fold, None)
        .await
        .unwrap();
    assert_eq!(stop, "end_turn");
    fold.flush().await.unwrap();
    let entries = store.replay(sid).await.unwrap();
    assert!(!entries.is_empty(), "transcript folded the notification");
}

#[tokio::test]
async fn test_child_request_ignored() {
    let (mut session, tx, _rx) = channel_session();
    let prompt_id = session.next_request_id();
    let req = r#"{"jsonrpc":"2.0","id":99,"method":"session/request_permission","params":{}}"#;
    tx.send(format!("{req}\n")).await.unwrap();
    let resp = format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{{"stopReason":"end_turn"}}}}"#,
        prompt_id
    );
    tx.send(format!("{resp}\n")).await.unwrap();
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let sid = SessionId::new();
    let mut fold = TranscriptFold::new(sid, store);
    let stop = session
        .recv_until_prompt(prompt_id, &mut fold, None)
        .await
        .unwrap();
    assert_eq!(stop, "end_turn");
}

#[tokio::test]
async fn test_pipe_close_during_recv() {
    let (mut session, tx, _rx) = channel_session();
    let prompt_id = session.next_request_id();
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let sid = SessionId::new();
    let mut fold = TranscriptFold::new(sid, store);
    drop(tx);
    let err = session
        .recv_until_prompt(prompt_id, &mut fold, None)
        .await
        .unwrap_err();
    assert!(matches!(err, AcpClientError::ChildExited(_)));
}

#[tokio::test]
async fn test_child_error_response() {
    let (mut session, tx, _rx) = channel_session();
    let prompt_id = session.next_request_id();
    let resp = format!(
        r#"{{"jsonrpc":"2.0","id":{},"error":{{"code":-32603,"message":"crashed"}}}}"#,
        prompt_id
    );
    tx.send(format!("{resp}\n")).await.unwrap();
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let sid = SessionId::new();
    let mut fold = TranscriptFold::new(sid, store);
    let err = session
        .recv_until_prompt(prompt_id, &mut fold, None)
        .await
        .unwrap_err();
    assert!(matches!(err, AcpClientError::Protocol(_)));
}

#[tokio::test]
async fn test_drive_prompt_returns_stop() {
    let (mut session, tx, _rx) = channel_session();
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let cancel = CancellationToken::new();
    let sid = SessionId::new();
    let notif = r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}}}"#;
    let resp = r#"{"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"}}"#;
    let task = tokio::spawn(async move {
        session
            .drive_prompt(sid, "go", &store, &cancel, None)
            .await
            .expect("prompt completes")
    });
    tx.send(format!("{notif}\n")).await.unwrap();
    tx.send(format!("{resp}\n")).await.unwrap();
    let stop = task.await.unwrap();
    assert_eq!(stop, "end_turn");
}

/// A cancel during drive_prompt sends session/cancel on the outbound channel.
#[tokio::test]
async fn test_cancel_sends_notification() {
    let (mut session, _tx, mut rx) = channel_session();
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let cancel = CancellationToken::new();
    let sid = SessionId::new();
    let cancel_clone = cancel.clone();
    let task =
        tokio::spawn(async move { session.drive_prompt(sid, "go", &store, &cancel, None).await });
    cancel_clone.cancel();
    let mut saw_cancel = false;
    for _ in 0..4 {
        let Some(frame) = rx.recv().await else { break };
        if frame.contains("session/cancel") {
            saw_cancel = true;
            break;
        }
    }
    drop(task.await);
    assert!(saw_cancel, "cancel notification was sent");
}

/// A launcher that hands back a child with pre-baked pipes.
struct MockLauncher {
    stdout_content: String,
}

impl ProcessLauncher for MockLauncher {
    fn spawn(&self, _req: SpawnRequest, _policy: SpawnPolicy) -> Result<LauncherChild, SpawnError> {
        let stdout = Box::new(Cursor::new(self.stdout_content.clone().into_bytes()));
        let stdin: Box<dyn Write + Send> = Box::new(sink());
        let pipes = StdioPipes {
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: None,
        };
        let waiter: houyicoder_async::PFut<'static, Result<LauncherExit, SpawnError>> =
            Box::pin(async {
                Ok(LauncherExit {
                    exit_code: Some(0),
                    stdout: None,
                    stderr: None,
                })
            });
        Ok(LauncherChild::with_pipes(Some(1), pipes, waiter))
    }
}

/// connect launches the process and wires its pipes into the pump.
#[tokio::test]
async fn test_connect_wires_pipes() {
    let launcher: Arc<dyn ProcessLauncher> = Arc::new(MockLauncher {
        stdout_content: String::new(),
    });
    let spec = ExternalAcpSpec {
        program: "test-prog".into(),
        args: vec!["--flag".into()],
        passthrough_env: vec!["TOKEN".into()],
        env: Vec::new(),
        handshake_timeout_ms: 100,
    };
    let session = AcpChildSession::connect(&launcher, &spec, &temp_dir(), &[]).unwrap();
    session.shutdown().await;
}

/// connect reports ConstructionFailed when the launcher errors.
#[tokio::test]
async fn test_connect_failed_reports_program() {
    struct FailingLauncher;
    impl ProcessLauncher for FailingLauncher {
        fn spawn(
            &self,
            _req: SpawnRequest,
            _policy: SpawnPolicy,
        ) -> Result<LauncherChild, SpawnError> {
            Err(SpawnError::Io("not found".into()))
        }
    }
    let launcher: Arc<dyn ProcessLauncher> = Arc::new(FailingLauncher);
    let spec = ExternalAcpSpec {
        program: "missing-prog".into(),
        args: Vec::new(),
        passthrough_env: Vec::new(),
        env: Vec::new(),
        handshake_timeout_ms: 100,
    };
    let result = AcpChildSession::connect(&launcher, &spec, &temp_dir(), &[]);
    match result {
        Err(AcpClientError::ConstructionFailed { program }) => assert_eq!(program, "missing-prog"),
        Err(other) => panic!("expected ConstructionFailed, got {other:?}"),
        Ok(_) => panic!("expected ConstructionFailed, got Ok"),
    }
}

/// connect reports ConstructionFailed when the child has no pipes.
#[tokio::test]
async fn test_connect_no_pipes_reports() {
    struct NoPipesLauncher;
    impl ProcessLauncher for NoPipesLauncher {
        fn spawn(
            &self,
            _req: SpawnRequest,
            _policy: SpawnPolicy,
        ) -> Result<LauncherChild, SpawnError> {
            let waiter: houyicoder_async::PFut<'static, Result<LauncherExit, SpawnError>> =
                Box::pin(async {
                    Ok(LauncherExit {
                        exit_code: Some(0),
                        stdout: None,
                        stderr: None,
                    })
                });
            Ok(LauncherChild::new(None, waiter))
        }
    }
    let launcher: Arc<dyn ProcessLauncher> = Arc::new(NoPipesLauncher);
    let spec = ExternalAcpSpec::default();
    let result = AcpChildSession::connect(&launcher, &spec, &temp_dir(), &[]);
    assert!(matches!(
        result,
        Err(AcpClientError::ConstructionFailed { .. })
    ));
}

/// A child permission ask with no fence is ignored, not answered.
#[tokio::test]
async fn test_ask_without_fence_ignored() {
    let (mut session, tx, _rx) = channel_session();
    let prompt_id = session.next_request_id();
    let ask = r#"{"jsonrpc":"2.0","id":99,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"call-1","title":"bash"},"options":[]}}"#;
    tx.send(format!("{ask}\n")).await.unwrap();
    let resp = format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{{"stopReason":"end_turn"}}}}"#,
        prompt_id
    );
    tx.send(format!("{resp}\n")).await.unwrap();
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let sid = SessionId::new();
    let mut fold = TranscriptFold::new(sid, store);
    let stop = session
        .recv_until_prompt(prompt_id, &mut fold, None)
        .await
        .unwrap();
    assert_eq!(stop, "end_turn");
}

/// A delegated ask publishes on the bus and the parent verdict returns to
/// the child as the option id the child offered.
#[tokio::test]
async fn test_delegated_ask_round_trip() {
    let bus: Arc<AgentBus> = Arc::new(Default::default());
    let root = temp_dir().join(format!("fence-rt-{}", id()));
    create_dir_all(&root).expect("root");
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::with_mode(PermissionMode::Manual));
    let fence = PermissionFence {
        workspace_root: root.clone(),
        gate: Some(gate),
        bus: Some(bus.clone()),
        pending: Arc::new(Mutex::new(Vec::new())),
        cancel: CancellationToken::new(),
        child_id: "c1".into(),
        agent_type: "review".into(),
    };
    let mut parent_rx = bus.subscribe(permission_request_topic());
    let (mut session, tx, mut rx) = channel_session();
    let prompt_id = session.next_request_id();
    let task = tokio::spawn(async move {
        session
            .recv_until_prompt(prompt_id, &mut fold_holder(), Some(&fence))
            .await
    });
    let ask = r#"{"jsonrpc":"2.0","id":99,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"call-1","title":"bash","rawInput":{"tool_name":"bash"}},"options":[{"optionId":"allow_once","name":"Allow once","kind":"allow_once"},{"optionId":"reject_once","name":"Reject once","kind":"reject_once"}]}}"#;
    tx.send(format!("{ask}\n")).await.unwrap();
    // Parent receives the delegated ask and approves it.
    let msg = parent_rx.recv().await.expect("delegated ask published");
    let BusMessage::PermissionRequest { call_id, .. } = msg else {
        panic!("expected a permission request");
    };
    bus.publish(
        &permission_response_topic("c1", &call_id),
        BusMessage::PermissionResponse {
            call_id,
            approved: true,
            updated_input: None,
            scope: "once".into(),
        },
    );
    // Child receives the allow answer.
    let answered = rx.recv().await.expect("answer sent to child");
    assert!(answered.contains(r#""id":99"#), "{answered}");
    assert!(answered.contains("allow_once"), "{answered}");
    let resp = format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{{"stopReason":"end_turn"}}}}"#,
        prompt_id
    );
    tx.send(format!("{resp}\n")).await.unwrap();
    let stop = task.await.unwrap().unwrap();
    assert_eq!(stop, "end_turn");
    remove_dir_all(&root).ok();
}

fn fold_holder() -> TranscriptFold {
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    TranscriptFold::new(SessionId::new(), store)
}

/// A stock client may issue a string id on its reverse ask. The answer comes
/// back under that same id; answering under a numeric id, or not at all,
/// leaves the child waiting for a reply that never matches its request.
#[tokio::test]
async fn test_ask_keeps_string_id() {
    let (mut session, tx, mut rx) = channel_session();
    let prompt_id = session.next_request_id();
    // Manual mode with no bus reaches the fail-closed answer, which is the
    // path that has to carry the child's id spelling back.
    let fence = PermissionFence {
        workspace_root: temp_dir(),
        gate: Some(Arc::new(DefaultModeGate::with_mode(PermissionMode::Manual))),
        bus: None,
        pending: Arc::new(Mutex::new(Vec::new())),
        cancel: CancellationToken::new(),
        child_id: "c1".into(),
        agent_type: "review".into(),
    };
    let task = tokio::spawn(async move {
        session
            .recv_until_prompt(prompt_id, &mut fold_holder(), Some(&fence))
            .await
    });
    let ask = r#"{"jsonrpc":"2.0","id":"ask-7","method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"call-1","title":"bash"},"options":[{"optionId":"reject_once","name":"Reject","kind":"reject_once"}]}}"#;
    tx.send(format!("{ask}\n")).await.unwrap();
    let answered = rx.recv().await.expect("an answer is sent");
    assert!(
        answered.contains(r#""id":"ask-7""#),
        "the reply keeps the child's own id: {answered}"
    );
    let resp =
        format!(r#"{{"jsonrpc":"2.0","id":{prompt_id},"result":{{"stopReason":"end_turn"}}}}"#);
    tx.send(format!("{resp}\n")).await.unwrap();
    let stop = task
        .await
        .expect("the receive task joined")
        .expect("the turn ends");
    assert_eq!(stop, "end_turn");
}

/// A stop while the child waits for the user ends the wait at once, so the
/// child gets its cancelled answer instead of sitting out the parent timeout.
#[tokio::test]
async fn test_stop_ends_parent_wait() {
    let (mut session, tx, mut rx) = channel_session();
    let prompt_id = session.next_request_id();
    let bus: Arc<AgentBus> = Arc::new(Default::default());
    // Subscribed before the run, so the published ask is the latch that says
    // the child has reached the parent wait.
    let mut asks = bus.subscribe(permission_request_topic());
    let stop = CancellationToken::new();
    let fence = PermissionFence {
        workspace_root: temp_dir(),
        gate: Some(Arc::new(DefaultModeGate::with_mode(PermissionMode::Manual))),
        bus: Some(bus),
        pending: Arc::new(Mutex::new(Vec::new())),
        cancel: stop.clone(),
        child_id: "c1".into(),
        agent_type: "review".into(),
    };
    let task = tokio::spawn(async move {
        session
            .recv_until_prompt(prompt_id, &mut fold_holder(), Some(&fence))
            .await
    });
    let ask = r#"{"jsonrpc":"2.0","id":9,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"call-1","title":"bash"},"options":[{"optionId":"allow_once","name":"Allow","kind":"allow_once"}]}}"#;
    tx.send(format!("{ask}\n")).await.unwrap();
    drop(
        asks.recv()
            .await
            .expect("the ask is published while the child waits"),
    );
    stop.cancel();
    let answered = rx.recv().await.expect("an answer is sent");
    assert!(
        answered.contains("cancelled"),
        "the waiting child is answered with a cancel: {answered}"
    );
    let resp =
        format!(r#"{{"jsonrpc":"2.0","id":{prompt_id},"result":{{"stopReason":"end_turn"}}}}"#);
    tx.send(format!("{resp}\n")).await.unwrap();
    drop(task.await);
}

/// Every wire stop reason reads as the word the summary and the fleet show,
/// so a model or a user never sees a Rust variant name.
#[test]
fn test_stop_reason_words() {
    assert_eq!(stop_reason_label(StopReason::EndTurn), "end_turn");
    assert_eq!(stop_reason_label(StopReason::MaxTokens), "max_tokens");
    assert_eq!(
        stop_reason_label(StopReason::MaxTurnRequests),
        "max_turn_requests"
    );
    assert_eq!(stop_reason_label(StopReason::Refusal), "refusal");
    assert_eq!(stop_reason_label(StopReason::Cancelled), "cancelled");
}

/// A child that ignores its closed stdin is killed after the grace instead of
/// holding the caller for as long as it wants.
#[tokio::test]
async fn test_shutdown_kills_unresponsive_child() {
    struct NeverExits;
    impl ProcessLauncher for NeverExits {
        fn spawn(
            &self,
            _req: houyicoder_api::launcher::SpawnRequest,
            _policy: houyicoder_api::launcher::SpawnPolicy,
        ) -> Result<LauncherChild, SpawnError> {
            let stdout = Box::new(Cursor::new(Vec::new()));
            let stdin: Box<dyn std::io::Write + Send> = Box::new(sink());
            let pipes = StdioPipes {
                stdin: Some(stdin),
                stdout: Some(stdout),
                stderr: None,
            };
            let waiter: houyicoder_async::PFut<'static, Result<LauncherExit, SpawnError>> =
                Box::pin(pending());
            Ok(LauncherChild::with_pipes(Some(1), pipes, waiter))
        }
    }
    let launcher: Arc<dyn ProcessLauncher> = Arc::new(NeverExits);
    let spec = ExternalAcpSpec {
        program: "never-exits".into(),
        args: vec![],
        passthrough_env: vec![],
        env: Vec::new(),
        handshake_timeout_ms: 100,
    };
    let session = AcpChildSession::connect(&launcher, &spec, &temp_dir(), &[])
        .expect("connect")
        .with_graces(TEST_GRACE, TEST_GRACE);
    let started = std::time::Instant::now();
    session.shutdown().await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "shutdown returns once the grace passes"
    );
}
