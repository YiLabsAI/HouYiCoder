//! Request rejection paths: every request must fail closed with a typed
//! error reply naming the mismatch or refusal, never a silent drop and
//! never a hang on the request id.

#![cfg(test)]

use super::*;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::sandbox::{SandboxSession, WorktreeFenceGuard};
use houyicoder_async::PFut;
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, DirEntry, EventId, ExecConfig,
    ExecResult, SandboxError, SessionId, SessionLogEntry,
};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::{
    Decision, DefaultModeGate, ModeError, ModeGate, PermissionMode, Rule, ToolRequest,
};
use houyicoder_protocol::envelope::{
    ClientFrame, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::frontend::SessionId as FrontendSessionId;
use houyicoder_protocol::frontend::permission::{
    PermissionEffect, PermissionRule, RuleDestination,
};
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::handshake::Hello;
use houyicoder_session::SessionStore;
use std::path::Path;
use std::sync::Arc;

/// A gate that delegates everything but the mode cycle, which always refuses.
/// The production gate never fails a cycle, so the dispatch error arm needs
/// this stub to be exercised.
struct FailingCycleGate {
    inner: DefaultModeGate,
}

impl ModeGate for FailingCycleGate {
    fn decide(&self, req: &ToolRequest) -> Decision {
        self.inner.decide(req)
    }
    fn current(&self) -> PermissionMode {
        self.inner.current()
    }
    fn set_mode(&self, new: PermissionMode, reason: &str) {
        self.inner.set_mode(new, reason);
    }
    fn tab_cycle(&self) -> Result<PermissionMode, ModeError> {
        Err(ModeError("stub cycle refusal".into()))
    }
    fn rules(&self) -> Vec<Rule> {
        self.inner.rules()
    }
    fn add_rule(&self, rule: Rule) {
        self.inner.add_rule(rule);
    }
    fn remove_rule(&self, index: usize) -> bool {
        self.inner.remove_rule(index)
    }
    fn set_git_checkpoint_enabled(&self, enabled: bool) {
        self.inner.set_git_checkpoint_enabled(enabled);
    }
    fn git_checkpoint_enabled(&self) -> bool {
        self.inner.git_checkpoint_enabled()
    }
}

/// A sandbox session whose fence cannot widen: add_working_dir keeps the
/// trait default, which refuses runtime-mutable widening.
struct StubFence;

impl SandboxSession for StubFence {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async move {
            Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }
    fn read_file(&self, _path: &str, _max: usize) -> PFut<'_, Result<Vec<u8>, SandboxError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn write_file(&self, _path: &str, _content: Vec<u8>) -> PFut<'_, Result<(), SandboxError>> {
        Box::pin(async move { Ok(()) })
    }
    fn list_dir(&self, _path: &str) -> PFut<'_, Result<Vec<DirEntry>, SandboxError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn path_exists(&self, _path: &str) -> PFut<'_, Result<bool, SandboxError>> {
        Box::pin(async move { Ok(false) })
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(std::path::PathBuf::from("/"))
    }
    fn narrow_to_worktree(
        &self,
        _worktree: &Path,
        _git_common_dir: &Path,
    ) -> Result<WorktreeFenceGuard, SandboxError> {
        Ok(WorktreeFenceGuard::new(Box::new(|| Ok(()))))
    }
    fn active_exec_count(&self) -> usize {
        0
    }
}

/// A context backend whose checkpoint write always fails, so a manual
/// /compact surfaces the storage failure instead of reporting success.
struct FailingCheckpoint;

impl ContextBackend for FailingCheckpoint {
    fn append(&self, _e: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        Box::pin(async { Ok(EventId::new()) })
    }
    fn read_range(
        &self,
        _s: SessionId,
        _from: Option<EventId>,
        _to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn replay(&self, _s: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn write_checkpoint(
        &self,
        _m: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        Box::pin(async { Err(ContextError::Io) })
    }
    fn read_checkpoint(
        &self,
        _id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        Box::pin(async { Err(ContextError::NotFound) })
    }
    fn list_checkpoints(&self, _s: SessionId) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        Box::pin(async { Ok(vec![]) })
    }
}

fn stub_runner() -> Arc<Runner> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Arc::new(Runner::with_shared_store(
        store,
        Arc::new(houyicoder_provider::FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    ))
}

fn failing_checkpoint_runner() -> Arc<Runner> {
    let store = Arc::new(SessionStore::new(Box::new(FailingCheckpoint)));
    Arc::new(Runner::with_shared_store(
        store,
        Arc::new(houyicoder_provider::FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    ))
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

/// Drive one request through a full serve loop and return the error reply.
/// Every test here expects the dispatch to answer with a typed error on the
/// request id, so the reply extraction is shared.
async fn error_reply(server: Server, request: FrontendRequest) -> ProtocolError {
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);

    send_line(
        &mut client_tx,
        &ClientFrame::Request(RequestEnvelope::new(RequestId(1), request)),
    );
    let err = match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => match r.payload {
            ResponsePayload::Error(e) => e,
            other => panic!("expected Error, got {other:?}"),
        },
        other => panic!("expected response, got {other:?}"),
    };
    handle.abort();
    err
}

fn plain_server() -> Server {
    Server::new(
        stub_runner(),
        SessionId::new(),
        Arc::new(DefaultModeGate::new()),
    )
}

fn bogus_text_send() -> FrontendRequest {
    FrontendRequest::MessageSend {
        session_id: FrontendSessionId::new("bogus"),
        content: vec![ContentBlock::Text { text: "hi".into() }],
        disabled_skills: Default::default(),
    }
}

/// A MessageSend naming a different session is refused, not run: the reply
/// names the offending id so a misrouted client can see its mistake.
#[tokio::test]
async fn test_message_send_session_mismatch() {
    let err = error_reply(plain_server(), bogus_text_send()).await;
    assert_eq!(err.category, ErrorCategory::InvalidRequest);
    assert_eq!(err.message, "session id mismatch: got bogus");
}

/// RunCancel carries the same session gate: cancelling someone else's
/// session is refused with the same typed mismatch.
#[tokio::test]
async fn test_run_cancel_session_mismatch() {
    let err = error_reply(
        plain_server(),
        FrontendRequest::RunCancel {
            session_id: FrontendSessionId::new("bogus"),
            reason: "test".into(),
        },
    )
    .await;
    assert_eq!(err.category, ErrorCategory::InvalidRequest);
    assert_eq!(err.message, "session id mismatch: got bogus");
}

/// A compact whose checkpoint write fails surfaces the storage error on the
/// request id instead of hanging the host on a reply that never comes.
#[tokio::test]
async fn test_compact_backend_failure() {
    let server = Server::new(
        failing_checkpoint_runner(),
        SessionId::new(),
        Arc::new(DefaultModeGate::new()),
    );
    let err = error_reply(server, FrontendRequest::Compact).await;
    assert_eq!(err.category, ErrorCategory::InvalidRequest);
    assert!(
        err.message.contains("context backend io error"),
        "the storage failure is named: {}",
        err.message
    );
}

/// A gate that refuses the mode cycle surfaces its refusal as a typed error
/// reply; the mode stays where it was.
#[tokio::test]
async fn test_cycle_mode_gate_error() {
    let server = Server::new(
        stub_runner(),
        SessionId::new(),
        Arc::new(FailingCycleGate {
            inner: DefaultModeGate::new(),
        }),
    );
    let err = error_reply(server, FrontendRequest::PermissionCycleMode).await;
    assert_eq!(err.category, ErrorCategory::InvalidRequest);
    assert_eq!(err.message, "stub cycle refusal");
}

/// A rule with an empty tool name never reaches the gate: the wire parse
/// refuses it and the reply names the reason.
#[tokio::test]
async fn test_add_rule_empty_action() {
    let err = error_reply(
        plain_server(),
        FrontendRequest::PermissionAddRule {
            rule: PermissionRule {
                action: " ".into(),
                content: None,
                effect: PermissionEffect::Allow,
                destination: RuleDestination::Session,
            },
        },
    )
    .await;
    assert_eq!(err.category, ErrorCategory::InvalidRequest);
    assert_eq!(err.message, "empty tool name");
}

/// Removing a rule index that does not exist is refused with a typed error
/// rather than a silent no-op the frontend would render as success.
#[tokio::test]
async fn test_remove_rule_bad_index() {
    let err = error_reply(
        plain_server(),
        FrontendRequest::PermissionRemoveRule { index: 9999 },
    )
    .await;
    assert_eq!(err.category, ErrorCategory::InvalidRequest);
    assert_eq!(err.message, "rule index out of range");
}

/// A fence that cannot widen refuses the directory add, and the refusal
/// reaches the wire as a typed error naming the runtime-mutable requirement.
#[tokio::test]
async fn test_working_dir_fence_error() {
    let server = plain_server().with_session(Arc::new(StubFence));
    let err = error_reply(
        server,
        FrontendRequest::PermissionAddWorkingDir {
            path: "/tmp".into(),
        },
    )
    .await;
    assert_eq!(err.category, ErrorCategory::InvalidRequest);
    assert!(
        err.message.contains("runtime-mutable"),
        "the refusal names the fence requirement: {}",
        err.message
    );
}

/// SessionReset carries the same session gate as the run verbs: resetting
/// someone else's session is refused with the typed mismatch.
#[tokio::test]
async fn test_session_reset_mismatch() {
    let err = error_reply(
        plain_server(),
        FrontendRequest::SessionReset {
            session_id: FrontendSessionId::new("bogus"),
        },
    )
    .await;
    assert_eq!(err.category, ErrorCategory::InvalidRequest);
    assert_eq!(err.message, "session id mismatch");
}
