//! Permission wire-verb contract tests: drive the server through its PUBLIC
//! frame interface (serve loop + ClientFrame::Request) — NOT the internal
//! dispatch() shortcut — so this exercises the full wire path a real client
//! uses (frame parse -> serve routing -> gate/sandbox -> response frame).
//! Integration tests (real Runner + Server + mocked provider + real gate),
//! so they live in tests/, not the --lib unit gate.
//!
//! Frame-based mirrors of the inline dispatch_* tests that used pub(super)
//! Server::dispatch directly. The rejection paths assert WireErrorKind::
//! InvalidRequest (a well-formed frame that is not a valid request for the
//! current state), not InvalidFrame.
//!
//! Coverage note: diff-cov (make check, --lib) does not see these tests'
//! coverage of server_dispatch.rs (they are not in the --lib lcov). That is
//! correct gate behavior — make check passes now (no implementation change ->
//! no diff to gate); the next change to server_dispatch.rs is required to
//! bring its changed lines to 80% lib coverage (the ratchet), which is the
//! gate's job, not a hole.

mod common;

use std::sync::{Arc, Mutex};

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_context::{DirEntry, ExecConfig, ExecResult, SandboxError, SessionId};
use houyicoder_permission::{DefaultModeGate, ModeGate};
use houyicoder_protocol::envelope::{
    ClientFrame, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::permission::{
    PermissionEffect, PermissionRule, PermissionRuleContent, RuleDestination,
};
use houyicoder_protocol::wire::WireErrorKind;
use houyicoder_service::server::Server;

use common::{pair, recv_frame, recv_hello, send_frame, stub_runner};

/// Spawn the server's serve loop + complete the Hello handshake. Returns the
/// client channel halves for the test to send requests + read responses.
async fn handshake(
    server: Server,
) -> (
    futures::channel::mpsc::Sender<String>,
    futures::channel::mpsc::Receiver<String>,
    tokio::task::JoinHandle<Result<(), houyicoder_protocol::wire::WireError>>,
) {
    let (io, mut client_tx, mut client_rx) = pair();
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_frame(
        &mut client_tx,
        &houyicoder_protocol::handshake::Hello::local(),
    )
    .await;
    let _ = recv_hello(&mut client_rx).await;
    (client_tx, client_rx, handle)
}

/// A no-op sandbox session that records the working-dir add/remove verbs so
/// the round-trip test can assert the list without a real seatbelt.
struct RecordingSession {
    dirs: Mutex<Vec<String>>,
}

impl SandboxSession for RecordingSession {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> houyicoder_async::PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async move {
            Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }
    fn read_file(
        &self,
        _path: &str,
        _max_bytes: usize,
    ) -> houyicoder_async::PFut<'_, Result<Vec<u8>, SandboxError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn write_file(
        &self,
        _path: &str,
        _content: Vec<u8>,
    ) -> houyicoder_async::PFut<'_, Result<(), SandboxError>> {
        Box::pin(async move { Ok(()) })
    }
    fn list_dir(
        &self,
        _path: &str,
    ) -> houyicoder_async::PFut<'_, Result<Vec<DirEntry>, SandboxError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn path_exists(&self, _path: &str) -> houyicoder_async::PFut<'_, Result<bool, SandboxError>> {
        Box::pin(async move { Ok(false) })
    }
    fn workspace_root(&self) -> std::sync::Arc<std::path::Path> {
        std::sync::Arc::from(std::path::PathBuf::from("/"))
    }
    fn add_working_dir(&self, path: &str) -> Result<(), SandboxError> {
        let mut dirs = self.dirs.lock().unwrap();
        if !dirs.iter().any(|d| d == path) {
            dirs.push(path.to_string());
        }
        Ok(())
    }
    fn remove_working_dir(&self, path: &str) {
        let mut dirs = self.dirs.lock().unwrap();
        dirs.retain(|d| d != path);
    }
    fn working_dirs(&self) -> Vec<String> {
        self.dirs.lock().unwrap().clone()
    }
}

/// A deny rule added via the wire verb must reach the gate AND a real tool
/// decision must honor it (Allow -> Deny). The UI add changes agent
/// behavior, not just the rules list.
#[tokio::test]
async fn test_add_rule_takes_effect() {
    use houyicoder_permission::{Outcome, ToolRequest};
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let gate_assert = gate.clone();
    let (runner, session) = stub_runner();
    let (mut client_tx, mut client_rx, _h) = handshake(Server::new(runner, session, gate)).await;

    let npm_req = ToolRequest {
        tool_name: "bash",
        input: Some(&serde_json::json!({"command": "npm test"})),
        is_destructive: false,
        is_read_only: false,
        native_requires_approval: false,
    };
    assert!(
        gate_assert.decide(&npm_req).outcome() == Outcome::Allow,
        "auto mode allows plain bash before the deny rule"
    );

    let rule = PermissionRule {
        action: "bash".into(),
        content: Some(PermissionRuleContent::Prefix {
            value: "npm".into(),
        }),
        effect: PermissionEffect::Reject,
        destination: RuleDestination::Project,
    };
    let req = RequestEnvelope::new(RequestId(11), FrontendRequest::PermissionAddRule { rule });
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            ResponsePayload::PermissionRules(_) => {}
            other => panic!("expected PermissionRules, got {other:?}"),
        },
        other => panic!("expected Response, got {other:?}"),
    }
    assert!(
        gate_assert.decide(&npm_req).outcome() == Outcome::Deny,
        "deny rule takes effect on a real decision after the wire add"
    );
}

/// Add + Remove working dir round-trip through the wire verbs; the
/// PermissionWorkingDirs reply carries the live list.
#[tokio::test]
async fn test_working_dir_round_trips() {
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let (runner, session) = stub_runner();
    let stub = Arc::new(RecordingSession {
        dirs: Mutex::new(Vec::new()),
    }) as Arc<dyn SandboxSession>;
    let server = Server::new(runner, session, gate).with_session(stub);
    let (mut client_tx, mut client_rx, _h) = handshake(server).await;

    let req = RequestEnvelope::new(
        RequestId(7),
        FrontendRequest::PermissionAddWorkingDir {
            path: "/tmp/extra".into(),
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            ResponsePayload::PermissionWorkingDirs(dirs) => {
                assert_eq!(dirs, vec!["/tmp/extra".to_string()], "list after add");
            }
            other => panic!("expected PermissionWorkingDirs, got {other:?}"),
        },
        other => panic!("expected Response, got {other:?}"),
    }
    let req = RequestEnvelope::new(
        RequestId(8),
        FrontendRequest::PermissionRemoveWorkingDir {
            path: "/tmp/extra".into(),
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            ResponsePayload::PermissionWorkingDirs(dirs) => {
                assert!(dirs.is_empty(), "list empty after remove: {dirs:?}");
            }
            other => panic!("expected PermissionWorkingDirs, got {other:?}"),
        },
        other => panic!("expected Response, got {other:?}"),
    }
}

/// A RemoveRule with an out-of-range index is a business rejection
/// (InvalidRequest), not InvalidFrame — the frame parsed fine.
#[tokio::test]
async fn test_remove_rule_oob_rejects() {
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let (runner, session) = stub_runner();
    let (mut client_tx, mut client_rx, _h) = handshake(Server::new(runner, session, gate)).await;
    let req = RequestEnvelope::new(
        RequestId(20),
        FrontendRequest::PermissionRemoveRule { index: 99 },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            ResponsePayload::Error(e) => {
                assert_eq!(e.kind, WireErrorKind::InvalidRequest);
                assert!(e.message.contains("range"), "msg: {}", e.message);
            }
            other => panic!("expected Error, got {other:?}"),
        },
        other => panic!("expected Response, got {other:?}"),
    }
}

/// PermissionAddWorkingDir with no sandbox session attached is a business
/// rejection (InvalidRequest) — the server has no runtime-mutable fence.
#[tokio::test]
async fn test_dir_no_session_rejects() {
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let (runner, session) = stub_runner();
    // No .with_session(...) — sandbox_session stays None.
    let (mut client_tx, mut client_rx, _h) = handshake(Server::new(runner, session, gate)).await;
    let req = RequestEnvelope::new(
        RequestId(21),
        FrontendRequest::PermissionAddWorkingDir {
            path: "/tmp".into(),
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            ResponsePayload::Error(e) => {
                assert_eq!(e.kind, WireErrorKind::InvalidRequest);
                assert!(e.message.contains("sandbox"), "msg: {}", e.message);
            }
            other => panic!("expected Error, got {other:?}"),
        },
        other => panic!("expected Response, got {other:?}"),
    }
}

/// A MessageSend with a session id that does not match the server's session
/// is a business rejection (InvalidRequest), not InvalidFrame.
#[tokio::test]
async fn test_message_wrong_session_rejects() {
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let (runner, _session) = stub_runner();
    let server_session = SessionId::new();
    let (mut client_tx, mut client_rx, _h) =
        handshake(Server::new(runner, server_session, gate)).await;
    let req = RequestEnvelope::new(
        RequestId(30),
        FrontendRequest::MessageSend {
            session_id: houyicoder_protocol::frontend::SessionId("wrong-session".into()),
            content: vec![houyicoder_protocol::frontend::run::ContentBlock::Text {
                text: "hi".into(),
            }],
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            ResponsePayload::Error(e) => {
                assert_eq!(e.kind, WireErrorKind::InvalidRequest);
                assert!(e.message.contains("session"), "msg: {}", e.message);
            }
            other => panic!("expected Error, got {other:?}"),
        },
        other => panic!("expected Response, got {other:?}"),
    }
}
