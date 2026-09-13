//! Successful frontend request contracts.
//!
//! Covers tool, skill, status, permission-rule, and working-directory
//! responses. Rejection contracts live in request_rejection_tests.

#![cfg(test)]

use super::*;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::sandbox::{SandboxSession, WorktreeFenceGuard};
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::{DirEntry, ExecConfig, ExecResult, SandboxError, SessionId};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::{
    ClientFrame, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::permission::{
    PermissionEffect, PermissionRule, RuleDestination,
};
use houyicoder_protocol::handshake::Hello;
use houyicoder_session::SessionStore;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A tool with a distinct name so the list request has something to return.
struct ListedTool;

impl Tool for ListedTool {
    fn name(&self) -> &str {
        "listed_probe"
    }
    fn description(&self) -> &str {
        "a tool the list request returns"
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async { Ok(serde_json::json!({})) })
    }
}

/// A fence that records the dirs added at runtime, so the working-dir add
/// succeeds and the reply reflects the widening. The Ok arm of the dispatch
/// branch is reachable only with a runtime-mutable fence attached; StubFence
/// in request_rejection_tests keeps the default that exercises the refusing
/// branch.
struct RecordingFence {
    dirs: Mutex<Vec<String>>,
}

impl RecordingFence {
    fn new() -> Self {
        Self {
            dirs: Mutex::new(Vec::new()),
        }
    }
}

impl SandboxSession for RecordingFence {
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
        Box::pin(async move { Ok(true) })
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(PathBuf::from("/"))
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
    fn add_working_dir(&self, path: &str) -> Result<(), SandboxError> {
        let mut dirs = self.dirs.lock().expect("dirs lock");
        if !dirs.iter().any(|d| d == path) {
            dirs.push(path.to_string());
        }
        Ok(())
    }
    fn remove_working_dir(&self, path: &str) {
        self.dirs.lock().expect("dirs lock").retain(|d| d != path);
    }
    fn working_dirs(&self) -> Vec<String> {
        self.dirs.lock().expect("dirs lock").clone()
    }
}

fn runner_with_tool() -> Arc<Runner> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ListedTool));
    Arc::new(Runner::with_shared_store(
        store,
        Arc::new(houyicoder_provider::FakeProvider::text("x")),
        tools,
        RunnerConfig::default(),
    ))
}

fn plain_server() -> Server {
    Server::new(
        runner_with_tool(),
        SessionId::new(),
        Arc::new(houyicoder_permission::DefaultModeGate::new()),
    )
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

/// Drive one connection: send every request in order, then collect the
/// replies. Order is preserved, so a multi-request drive reads as a sequence
/// of state changes against the same server.
async fn dispatch_responses(server: Server, reqs: Vec<FrontendRequest>) -> Vec<ResponsePayload> {
    let (server_tx, mut client_rx) = mpsc::channel::<String>(256);
    let (mut client_tx, server_rx) = mpsc::channel::<String>(256);
    let io = FrameCarrier::new(server_tx, server_rx);
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_line(&mut client_tx, &Hello::local());
    drop(client_rx.next().await);
    let count = reqs.len();
    for (i, payload) in reqs.into_iter().enumerate() {
        let req = ClientFrame::Request(RequestEnvelope::new(RequestId(i as u64 + 1), payload));
        send_line(&mut client_tx, &req);
    }
    let mut payloads = Vec::new();
    for _ in 0..count {
        match recv_frame(&mut client_rx).await {
            ServerFrame::Response(r) => payloads.push(r.payload),
            other => panic!("expected response, got {other:?}"),
        }
    }
    handle.abort();
    payloads
}

/// ToolList returns the registered tools as a Tools payload, so the picker
/// renders what the runner actually holds.
#[tokio::test]
async fn test_tool_list_returns_tools() {
    let payloads = dispatch_responses(plain_server(), vec![FrontendRequest::ToolList]).await;
    match &payloads[0] {
        ResponsePayload::Tools(tools) => assert!(
            tools.iter().any(|t| t.name == "listed_probe"),
            "the registered tool is listed: {tools:?}"
        ),
        other => panic!("expected Tools, got {other:?}"),
    }
}

/// Skills returns the runner's skill snapshot as a Skills payload; the stub
/// runner has no registry installed, so the snapshot is empty but the branch
/// still projects it to the protocol form.
#[tokio::test]
async fn test_skills_returns_payload() {
    let payloads = dispatch_responses(plain_server(), vec![FrontendRequest::Skills]).await;
    assert!(
        matches!(&payloads[0], ResponsePayload::Skills(_)),
        "expected Skills, got {:?}",
        payloads[0]
    );
}

/// Status builds a full snapshot and attaches the running build version,
/// proving the server-side status projection ran to completion (env display
/// fields, the auto-memory/auto-dream toggles, and the per-model usage).
#[tokio::test]
async fn test_status_builds_snapshot() {
    let payloads = dispatch_responses(plain_server(), vec![FrontendRequest::Status]).await;
    match &payloads[0] {
        ResponsePayload::Status(snapshot) => {
            assert!(
                !snapshot.version.is_empty(),
                "the build version is attached"
            );
        }
        other => panic!("expected Status, got {other:?}"),
    }
}

/// Adding a durable rule then removing it: the add reply carries the new rule
/// set and the remove reply carries it gone, so the /permissions list stays
/// in sync without a poll. The index is against the writable rule set, the
/// same projection the reply ships.
#[tokio::test]
async fn test_rule_add_then_remove() {
    let rule = PermissionRule {
        action: "Bash".into(),
        content: None,
        effect: PermissionEffect::Allow,
        destination: RuleDestination::Project,
    };
    let payloads = dispatch_responses(
        plain_server(),
        vec![
            FrontendRequest::PermissionAddRule { rule },
            FrontendRequest::PermissionRemoveRule { index: 0 },
        ],
    )
    .await;
    match &payloads[0] {
        ResponsePayload::PermissionRules(rules) => {
            assert_eq!(rules.len(), 1, "the added rule is in the set: {rules:?}");
            assert_eq!(rules[0].action, "Bash", "the added rule keeps its action");
        }
        other => panic!("expected PermissionRules after add, got {other:?}"),
    }
    match &payloads[1] {
        ResponsePayload::PermissionRules(rules) => {
            assert!(rules.is_empty(), "the removed rule is gone: {rules:?}");
        }
        other => panic!("expected PermissionRules after remove, got {other:?}"),
    }
}

/// Widening a runtime-mutable fence then narrowing it: the add reply carries
/// the new dir and the remove reply carries it gone, so the Workspace tab
/// reflects the fence without a poll.
#[tokio::test]
async fn test_working_dir_add_remove() {
    let server = plain_server().with_session(Arc::new(RecordingFence::new()));
    let payloads = dispatch_responses(
        server,
        vec![
            FrontendRequest::PermissionAddWorkingDir {
                path: "/workspace/extra".into(),
            },
            FrontendRequest::PermissionRemoveWorkingDir {
                path: "/workspace/extra".into(),
            },
        ],
    )
    .await;
    match &payloads[0] {
        ResponsePayload::PermissionWorkingDirs(dirs) => assert_eq!(
            dirs,
            &["/workspace/extra".to_string()],
            "the added dir is listed"
        ),
        other => panic!("expected PermissionWorkingDirs after add, got {other:?}"),
    }
    match &payloads[1] {
        ResponsePayload::PermissionWorkingDirs(dirs) => {
            assert!(dirs.is_empty(), "the removed dir is gone: {dirs:?}");
        }
        other => panic!("expected PermissionWorkingDirs after remove, got {other:?}"),
    }
}
