//! Tests for session lifecycle and transcript folding.

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::io::Cursor;
use std::io::Write;
use std::io::sink;
use std::path::Path;
use std::process::id;
use std::sync::Arc;

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

fn channel_session() -> (
    AcpChildSession,
    mpsc::Sender<String>,
    mpsc::Receiver<String>,
) {
    let (client_tx, server_rx) = mpsc::channel::<String>(16);
    let (server_tx, client_rx) = mpsc::channel::<String>(16);
    let pump = from_line_channels(server_rx, server_tx);
    (AcpChildSession::from_pump(pump), client_tx, client_rx)
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

/// Banner lines before the first response are skipped during handshake.
#[tokio::test]
async fn test_banner_skipped_during_handshake() {
    let (mut session, tx, mut rx) = channel_session();
    let workspace = Path::new("/tmp");
    let task = tokio::spawn(async move { session.handshake_inner(workspace).await });
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
    let workspace = Path::new("/tmp");
    let task = tokio::spawn(async move {
        session
            .handshake_inner(workspace)
            .await
            .expect("handshake completes")
    });
    tx.send(format!("{init_resp}\n")).await.unwrap();
    await_outbound(&mut rx, "session/new").await;
    tx.send(format!("{new_resp}\n")).await.unwrap();
    let sid = task.await.unwrap();
    assert!(!sid.to_string().is_empty());
}

/// The handshake timeout fires when the child does not respond in time.
#[tokio::test]
async fn test_handshake_times_out() {
    let (mut session, _tx, _rx) = channel_session();
    let spec = ExternalAcpSpec {
        program: "test".into(),
        args: Vec::new(),
        passthrough_env: Vec::new(),
        handshake_timeout_ms: 50,
    };
    let workspace = Path::new("/tmp");
    let err = session
        .handshake(&spec, workspace)
        .await
        .expect_err("must time out");
    assert!(matches!(err, AcpClientError::HandshakeTimedOut));
}

/// A malformed initialize response surfaces as a Protocol error.
#[tokio::test]
async fn test_handshake_bad_init() {
    let (mut session, tx, _rx) = channel_session();
    let bad_resp = r#"{"jsonrpc":"2.0","id":1,"result":"not an object"}"#;
    let workspace = Path::new("/tmp");
    let task = tokio::spawn(async move { session.handshake_inner(workspace).await });
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
    assert_eq!(stop, "EndTurn");
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
    assert_eq!(stop, "EndTurn");
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
    assert_eq!(stop, "EndTurn");
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
        handshake_timeout_ms: 100,
    };
    let session = AcpChildSession::connect(&launcher, &spec, Path::new("/tmp"), &[]).unwrap();
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
        handshake_timeout_ms: 100,
    };
    let result = AcpChildSession::connect(&launcher, &spec, Path::new("/tmp"), &[]);
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
    let result = AcpChildSession::connect(&launcher, &spec, Path::new("/tmp"), &[]);
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
    assert_eq!(stop, "EndTurn");
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
        gate,
        bus: Some(bus.clone()),
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
    assert_eq!(stop, "EndTurn");
    remove_dir_all(&root).ok();
}

fn fold_holder() -> TranscriptFold {
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    TranscriptFold::new(SessionId::new(), store)
}
