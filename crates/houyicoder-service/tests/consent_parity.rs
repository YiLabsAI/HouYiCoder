//! Dual-transport consent parity: the same ask answered the same way must
//! produce the same approval options, installed rules, sandbox directory
//! grants, and permission audit over the frontend envelope transport and the
//! ACP transport, so a change that makes one transport differ reddens here.
//! One row is deliberately asymmetric: a lifetime the card did not offer is
//! reduced to a single approval by the trusted in-process client and refused
//! by the wire client. A child's ask over the agent bus and the disconnect
//! that settles a running turn ride in their own tests.

#![allow(clippy::too_many_lines)] // the corpus reads as one table

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::sandbox::{Containment, Coverage, FenceCapability, SandboxSession, SideEffect};
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_async::bus::MessageBus;
use houyicoder_context::{
    DirEntry, ExecConfig, ExecResult, SandboxError, SessionEvent, SessionId, SessionLogEntry,
};
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, permission_request_topic, permission_response_topic,
};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::{
    DefaultModeGate, ModeGate, NETWORK_EGRESS, Rule, RuleStore, StoreError,
};
use houyicoder_protocol::acp_wire::{
    AcpRequest, AcpRequestId, AcpResponse, PermissionOption, RequestPermissionOutcome,
    RequestPermissionResponse, SelectedPermissionOutcome,
};
use houyicoder_protocol::acpx::AcpxCapabilities;
use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponseEnvelope, ClientResponsePayload, RequestEnvelope, RequestId,
    ServerFrame, ServerRequestPayload,
};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::permission::{AskReason, AskSource};
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalScope, ContentBlock};
use houyicoder_protocol::frontend::{FrontendRequest, SessionId as FrontendSessionId};
use houyicoder_protocol::handshake::Hello;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use houyicoder_provider::FakeProvider;
use houyicoder_service::acp::adapter::AcpAdapter;
use houyicoder_service::acp::pump::from_line_channels;
use houyicoder_service::acp::server::AcpServer;
use houyicoder_service::lifecycle::SessionLeaseStore;
use houyicoder_service::server::{FrameCarrier, Server};
use houyicoder_session::SessionStore;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::sync::broadcast::Receiver as BroadcastReceiver;
use tokio::sync::mpsc as tokio_mpsc;
use tokio::sync::oneshot;
use tokio::time::sleep;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);

/// A sandbox that records the directory capabilities an install grants, and
/// reports whether a fence confines writes.
struct RecordingSandbox {
    root: PathBuf,
    reading: Mutex<Vec<String>>,
    writing: Mutex<Vec<String>>,
    fenced: bool,
}

impl RecordingSandbox {
    fn new(root: PathBuf, fenced: bool) -> Self {
        Self {
            root,
            reading: Mutex::new(Vec::new()),
            writing: Mutex::new(Vec::new()),
            fenced,
        }
    }

    /// Every directory the session holds, read-only and writable, sorted so
    /// the two transports compare the same set regardless of install order.
    fn dirs(&self) -> Vec<String> {
        let mut dirs = self.reading.lock().expect("reading dirs").clone();
        dirs.extend(self.writing.lock().expect("writing dirs").clone());
        dirs.sort();
        dirs
    }
}

impl SandboxSession for RecordingSandbox {
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
    fn read_file(&self, _path: &str, _max_bytes: usize) -> PFut<'_, Result<Vec<u8>, SandboxError>> {
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
        Arc::from(self.root.as_path())
    }
    fn add_working_dir(&self, path: &str) -> Result<(), SandboxError> {
        self.writing.lock().expect("writing dirs").push(path.into());
        Ok(())
    }
    fn add_reading_dir(&self, path: &str) -> Result<(), SandboxError> {
        self.reading.lock().expect("reading dirs").push(path.into());
        Ok(())
    }
    fn remove_working_dir(&self, path: &str) {
        self.writing
            .lock()
            .expect("writing dirs")
            .retain(|d| d != path);
        self.reading
            .lock()
            .expect("reading dirs")
            .retain(|d| d != path);
    }
    fn working_dirs(&self) -> Vec<String> {
        self.writing.lock().expect("writing dirs").clone()
    }
    fn reading_dirs(&self) -> Vec<String> {
        self.reading.lock().expect("reading dirs").clone()
    }
    fn writing_dirs(&self) -> Vec<String> {
        self.writing.lock().expect("writing dirs").clone()
    }
}

impl Containment for RecordingSandbox {
    fn coverage(&self) -> Coverage {
        Coverage::Unfenced
    }
    fn capability(&self) -> FenceCapability {
        if self.fenced {
            FenceCapability::filesystem()
        } else {
            FenceCapability::NONE
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
}

/// A rule store whose every write fails, so an approved rule degrades to the
/// session lifetime and the server reports it.
struct FailingStore;

impl RuleStore for FailingStore {
    fn try_load(&self) -> Result<Vec<Rule>, StoreError> {
        Ok(Vec::new())
    }
    fn add(&self, _rule: &Rule) -> Result<(), StoreError> {
        Err(StoreError::Io)
    }
    fn remove(&self, _rule: &Rule) -> Result<(), StoreError> {
        Err(StoreError::Io)
    }
    fn clear(&self) -> Result<(), StoreError> {
        Err(StoreError::Io)
    }
    fn try_load_directories(&self) -> Result<Vec<PathBuf>, StoreError> {
        Ok(Vec::new())
    }
    fn try_load_read_directories(&self) -> Result<Vec<PathBuf>, StoreError> {
        Ok(Vec::new())
    }
}

/// A tool that declares the scenario's approval cause for every input, so the
/// engine raises exactly the ask the corpus describes.
struct AskingTool {
    name: &'static str,
    reason: AskReason,
}

impl Tool for AskingTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "a tool that asks before running"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move { Ok(json!({"ok": true})) })
    }
    fn approval_reason(&self, _input: &Value) -> Option<AskReason> {
        Some(self.reason.clone())
    }
}

/// One corpus row: the ask, the fence the session runs under, the answer the
/// client gives, and whether the audit may differ between a trusted
/// in-process client and an untrusted wire client.
struct Scenario {
    label: &'static str,
    tool: &'static str,
    input: Value,
    reason: AskReason,
    fenced: bool,
    failing_store: bool,
    answer: Answer,
    escalation: bool,
}

#[derive(Clone, Copy)]
struct Answer {
    approved: bool,
    scope: ApprovalScope,
}

impl Answer {
    fn option_id(self) -> &'static str {
        match (self.approved, self.scope) {
            (false, _) => "reject_once",
            (true, ApprovalScope::Once) => "allow_once",
            (true, _) => "allow_always",
        }
    }
}

/// The comparable facts a transport produces for one scenario.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    options: Vec<OptionView>,
    rules: Vec<String>,
    dirs: Vec<String>,
    audit: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct OptionView {
    id: String,
    name: String,
    scope: Option<String>,
}

impl OptionView {
    fn summarize(option: &PermissionOption) -> Self {
        Self {
            id: option.option_id.clone(),
            name: option.name.clone(),
            scope: option.scope.map(|s| format!("{s:?}")),
        }
    }
}

fn detection(validator: &str, detail: &str) -> AskReason {
    AskReason {
        source: AskSource::Detection,
        validator: validator.to_string(),
        detail: detail.to_string(),
        containment_note: None,
    }
}

fn text_response(text: &str) -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::Text { text: text.into() }],
        usage: Usage::default(),
        model: "test".into(),
    }
}

fn tool_call_response(name: &str, input: Value) -> CompletionResponse {
    CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "toolu_parity".into(),
            name: name.into(),
            input,
        }],
        usage: Usage::default(),
        model: "test".into(),
    }
}

/// The fixture a scenario runs against: one engine, one gate, one sandbox.
struct Fixture {
    runner: Arc<Runner>,
    gate: Arc<DefaultModeGate>,
    sandbox: Arc<RecordingSandbox>,
    session: SessionId,
}

fn fixture(scenario: &Scenario, outside: &Path) -> Fixture {
    let sandbox = Arc::new(RecordingSandbox::new(
        outside.join("workspace"),
        scenario.fenced,
    ));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(AskingTool {
        name: scenario.tool,
        reason: scenario.reason.clone(),
    }));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![
        tool_call_response(scenario.tool, scenario.input.clone()),
        text_response("done"),
    ]));
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let runner = Runner::new(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    )
    .with_sandbox_session(Some(sandbox.clone() as Arc<dyn SandboxSession>));
    // The gate's fence is what the card reads, so the fixture attaches the
    // same sandbox the tools run under, as the composition root does.
    let mut gate = DefaultModeGate::new().with_containment(sandbox.clone() as Arc<dyn Containment>);
    if scenario.failing_store {
        gate = gate.with_store(Arc::new(FailingStore));
    }
    Fixture {
        runner: Arc::new(runner),
        gate: Arc::new(gate),
        sandbox,
        session,
    }
}

/// The durable facts a finished transport produced, read back from the same
/// surfaces a client would use: the gate's rule set, the sandbox's directory
/// grants, and the session's permission audit.
async fn collect_outcome(fixture: &Fixture, options: Vec<OptionView>) -> Outcome {
    let rules: Vec<String> = fixture
        .gate
        .rules()
        .iter()
        .map(|rule| format!("{rule:?}"))
        .collect();
    let entries = fixture
        .runner
        .store()
        .replay(fixture.session)
        .await
        .expect("session replays");
    let audit = entries.iter().filter_map(permission_audit).collect();
    Outcome {
        options,
        rules,
        dirs: fixture.sandbox.dirs(),
        audit,
    }
}

fn permission_audit(entry: &SessionLogEntry) -> Option<String> {
    let SessionEvent::PermissionDecision {
        tool,
        verdict,
        scope,
        validator,
        reason,
        ..
    } = &entry.event
    else {
        return None;
    };
    Some(format!(
        "{tool}|{}|{scope}|{validator:?}|{reason:?}",
        verdict.label()
    ))
}

/// Drive one corpus row over ACP and read back what it left.
async fn run_acp(fixture: Fixture, scenario: &Scenario) -> Outcome {
    let (client_tx, server_rx) = tokio_mpsc::channel::<String>(256);
    let (server_tx, mut client_rx) = tokio_mpsc::channel::<String>(256);
    let mut io = from_line_channels(server_rx, server_tx);
    let adapter = Arc::new(AcpAdapter::new(
        AcpxCapabilities::default(),
        1,
        SessionLeaseStore::new(),
    ));
    let session = fixture.session;
    let server = AcpServer::new(
        adapter,
        fixture.runner.clone(),
        fixture.gate.clone(),
        session,
    );
    let handle = tokio::spawn(async move { server.serve(&mut io).await });

    send_line(&client_tx, &prompt_request(session)).await;
    let (ask_id, options) = recv_acp_ask(&mut client_rx).await;
    let answer = AcpResponse::ok(
        AcpRequestId::Number(ask_id),
        serde_json::to_value(RequestPermissionResponse {
            outcome: RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                option_id: scenario.answer.option_id().into(),
                meta: None,
            }),
            meta: None,
        })
        .expect("permission response serializes"),
    );
    send_line(&client_tx, &answer).await;
    recv_acp_reply(&mut client_rx).await;

    drop(client_tx);
    handle
        .await
        .expect("server task joins")
        .expect("server serves");
    let options = options.iter().map(OptionView::summarize).collect();
    collect_outcome(&fixture, options).await
}

/// Drive one corpus row over the frontend envelope and read back what it left.
async fn run_frontend(fixture: Fixture, scenario: &Scenario) -> Outcome {
    let (io, mut client_tx, mut client_rx) = pair();
    let session = fixture.session;
    let server = Server::new(fixture.runner.clone(), session, fixture.gate.clone());
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_frame(&mut client_tx, &Hello::local()).await;
    drop(recv_line(&mut client_rx).await);

    let req = RequestEnvelope::new(
        RequestId(7),
        FrontendRequest::MessageSend {
            session_id: FrontendSessionId::new(session.to_string()),
            content: vec![ContentBlock::Text { text: "go".into() }],
            disabled_skills: Default::default(),
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;

    let (ask_id, call_id, options) = recv_frontend_ask(&mut client_rx).await;
    let answer = ClientFrame::Response(ClientResponseEnvelope::new(
        ask_id,
        ClientResponsePayload::Permission(ApprovalDecision {
            call_id,
            approved: scenario.answer.approved,
            updated_input: None,
            scope: scenario.answer.scope,
        }),
    ));
    send_frame(&mut client_tx, &answer).await;
    recv_frontend_reply(&mut client_rx).await;

    drop(client_tx);
    handle
        .await
        .expect("server task joins")
        .expect("server serves");
    let options = options.iter().map(OptionView::summarize).collect();
    collect_outcome(&fixture, options).await
}

async fn recv_acp_ask(rx: &mut tokio_mpsc::Receiver<String>) -> (i64, Vec<PermissionOption>) {
    loop {
        let line = timeout(WAIT, rx.recv())
            .await
            .expect("acp frame timeout")
            .expect("acp frame");
        if !line.contains("session/request_permission") {
            continue;
        }
        let ask: AcpRequest = serde_json::from_str(line.trim_end()).expect("ask decodes");
        let params = ask.params.clone().expect("ask params");
        let options: Vec<PermissionOption> =
            serde_json::from_value(params["options"].clone()).expect("options decode");
        let AcpRequestId::Number(id) = ask.id else {
            panic!("permission ask carries a numeric id");
        };
        return (id, options);
    }
}

async fn recv_acp_reply(rx: &mut tokio_mpsc::Receiver<String>) {
    loop {
        let line = timeout(WAIT, rx.recv())
            .await
            .expect("acp reply timeout")
            .expect("acp reply");
        let Ok(AcpResponse::Result { result, .. }) = serde_json::from_str::<AcpResponse>(&line)
        else {
            continue;
        };
        if result.get("stopReason").is_some() {
            return;
        }
    }
}

async fn recv_frontend_ask(
    rx: &mut mpsc::Receiver<String>,
) -> (RequestId, String, Vec<PermissionOption>) {
    loop {
        let frame = recv_frame(rx).await;
        if let ServerFrame::Request(ask) = frame
            && let ServerRequestPayload::Permission(approval) = ask.payload
        {
            return (ask.req_id, approval.call_id, approval.options);
        }
    }
}

async fn recv_frontend_reply(rx: &mut mpsc::Receiver<String>) {
    loop {
        if let ServerFrame::Response(response) = recv_frame(rx).await
            && response.req_id == RequestId(7)
        {
            return;
        }
    }
}

fn pair() -> (FrameCarrier, mpsc::Sender<String>, mpsc::Receiver<String>) {
    let (client_tx, server_rx) = mpsc::channel::<String>(256);
    let (server_tx, client_rx) = mpsc::channel::<String>(256);
    (
        FrameCarrier::new(server_tx, server_rx),
        client_tx,
        client_rx,
    )
}

async fn send_frame(tx: &mut mpsc::Sender<String>, msg: &impl serde::Serialize) {
    let mut frame = encode(msg).expect("encode");
    if !frame.ends_with('\n') {
        frame.push('\n');
    }
    tx.send(frame).await.expect("client sends frame");
}

async fn recv_line(rx: &mut mpsc::Receiver<String>) -> String {
    timeout(WAIT, rx.next())
        .await
        .expect("frame timeout")
        .expect("server frame")
        .trim_end()
        .to_string()
}

async fn recv_frame(rx: &mut mpsc::Receiver<String>) -> ServerFrame {
    serde_json::from_str(&recv_line(rx).await).expect("frame decodes")
}

async fn send_line(tx: &tokio_mpsc::Sender<String>, msg: &impl serde::Serialize) {
    let mut frame = encode(msg).expect("encode");
    if !frame.ends_with('\n') {
        frame.push('\n');
    }
    tx.send(frame).await.expect("acp client sends frame");
}

fn prompt_request(session: SessionId) -> AcpRequest {
    AcpRequest::new(
        1,
        "session/prompt",
        json!({
            "sessionId": session.to_string(),
            "prompt": [{"type": "text", "text": "go"}],
        }),
    )
}

/// Each corpus row answered the same way over both transports must leave the
/// same options, rules, directory grants, and audit behind.
#[tokio::test]
async fn test_consent_parity_corpus() {
    let outside = scratch_dir("corpus");
    create_dir_all(outside.join("workspace")).expect("workspace dir");
    create_dir_all(outside.join("shared")).expect("shared dir");
    let shared_file = outside
        .join("shared")
        .join("report.txt")
        .display()
        .to_string();

    for scenario in corpus(&shared_file) {
        let acp = run_acp(fixture(&scenario, &outside), &scenario).await;
        let frontend = run_frontend(fixture(&scenario, &outside), &scenario).await;
        assert_eq!(
            acp.options, frontend.options,
            "{}: the transports offered different options",
            scenario.label
        );
        assert_eq!(
            acp.rules, frontend.rules,
            "{}: the transports installed different rules",
            scenario.label
        );
        assert_eq!(
            acp.dirs, frontend.dirs,
            "{}: the transports granted different directories",
            scenario.label
        );
        // The absolute shape each row must have, so a change that makes both
        // transports agree on the wrong answer still reddens for the rows
        // that carry a memory of what was granted.
        let (always, dirs, scope) = expected_shape(scenario.label);
        assert_eq!(
            always,
            acp.options.iter().any(|option| option.id == "allow_always"),
            "{}: allow-always offered as expected",
            scenario.label
        );
        assert_eq!(
            dirs,
            !acp.dirs.is_empty(),
            "{}: directory grant installed as expected",
            scenario.label
        );
        assert!(
            acp.audit
                .iter()
                .any(|entry| entry.contains(&format!("|{scope}|"))),
            "{}: audit records the {scope} lifetime: {:?}",
            scenario.label,
            acp.audit
        );
        match scenario.label {
            "outside directory" => assert!(
                acp.dirs.len() == 1 && Path::new(&acp.dirs[0]).ends_with("shared"),
                "the directory grant names the authorized directory: {:?}",
                acp.dirs
            ),
            "remembered command" | "rule save failure" => assert!(
                acp.rules.iter().any(|rule| rule.contains("cargo test")),
                "the rule names the remembered prefix: {:?}",
                acp.rules
            ),
            _ => {}
        }
        if scenario.escalation {
            // A trusted in-process client may ask for a lifetime the plan does
            // not carry, and the install reduces it to once. A wire client is
            // untrusted: an option the card did not offer is refused outright,
            // so no standing consent is created either way.
            assert!(
                frontend
                    .audit
                    .iter()
                    .any(|entry| entry.contains("|approved|once|")),
                "{}: frontend reduces the answer to once: {:?}",
                scenario.label,
                frontend.audit
            );
            assert!(
                acp.audit
                    .iter()
                    .any(|entry| entry.contains("|denied|once|")),
                "{}: acp refuses an unoffered option: {:?}",
                scenario.label,
                acp.audit
            );
        } else {
            assert_eq!(
                acp.audit, frontend.audit,
                "{}: the transports recorded different audits",
                scenario.label
            );
        }
    }
    remove_dir_all(&outside).ok();
}

/// What each row must produce, independent of the other transport: whether the
/// card offers a durable option, whether a directory grant lands, and the
/// lifetime the audit records.
fn expected_shape(label: &str) -> (bool, bool, &'static str) {
    match label {
        "one-shot command" => (false, false, "once"),
        "remembered command" => (true, false, "workspace"),
        "network egress" => (true, false, "session"),
        "git history" => (true, false, "session"),
        "outside directory" => (true, true, "workspace"),
        "unfenced session" => (false, false, "once"),
        "unoffered always" => (false, false, "once"),
        "rule save failure" => (true, false, "session"),
        other => panic!("no expectation for {other}"),
    }
}

/// The corpus rows. The child-ask and disconnect behaviors ride in their own
/// tests below because neither is a turn.
fn corpus(shared_file: &str) -> Vec<Scenario> {
    vec![
        Scenario {
            label: "one-shot command",
            tool: "bash",
            input: json!({"command": "cargo build && cargo test"}),
            reason: detection("bash", "running two commands"),
            fenced: true,
            failing_store: false,
            answer: Answer {
                approved: true,
                scope: ApprovalScope::Once,
            },
            escalation: false,
        },
        Scenario {
            label: "remembered command",
            tool: "bash",
            input: json!({"command": "cargo test --all"}),
            reason: detection("bash", "running a command"),
            fenced: true,
            failing_store: false,
            answer: Answer {
                approved: true,
                scope: ApprovalScope::Workspace,
            },
            escalation: false,
        },
        Scenario {
            label: "network egress",
            tool: "bash",
            input: json!({"command": "curl https://example.com/report"}),
            reason: detection(NETWORK_EGRESS, "reaching a remote host"),
            fenced: true,
            failing_store: false,
            answer: Answer {
                approved: true,
                scope: ApprovalScope::Session,
            },
            escalation: false,
        },
        Scenario {
            label: "git history",
            tool: "bash",
            input: json!({"command": "git commit -m change"}),
            reason: detection("git-checkpoint", "moving history"),
            fenced: true,
            failing_store: false,
            answer: Answer {
                approved: true,
                scope: ApprovalScope::Session,
            },
            escalation: false,
        },
        Scenario {
            label: "outside directory",
            tool: "write",
            input: json!({"path": shared_file}),
            reason: detection("path-bounds", "outside the workspace"),
            fenced: true,
            failing_store: false,
            answer: Answer {
                approved: true,
                scope: ApprovalScope::Workspace,
            },
            escalation: false,
        },
        Scenario {
            label: "unfenced session",
            tool: "bash",
            input: json!({"command": "curl https://example.com/report > report.json"}),
            reason: detection(NETWORK_EGRESS, "reaching a remote host"),
            fenced: false,
            failing_store: false,
            answer: Answer {
                approved: true,
                scope: ApprovalScope::Once,
            },
            escalation: false,
        },
        Scenario {
            label: "unoffered always",
            tool: "bash",
            input: json!({"command": "cargo build && cargo test"}),
            reason: detection("bash", "running two commands"),
            fenced: true,
            failing_store: false,
            answer: Answer {
                approved: true,
                scope: ApprovalScope::Workspace,
            },
            escalation: true,
        },
        Scenario {
            label: "rule save failure",
            tool: "bash",
            input: json!({"command": "cargo test --all"}),
            reason: detection("bash", "running a command"),
            fenced: true,
            failing_store: true,
            answer: Answer {
                approved: true,
                scope: ApprovalScope::Workspace,
            },
            escalation: false,
        },
    ]
}

/// A child's ask routed over the agent bus reaches the client on both
/// transports with the same options, rule install, and audit, and the same
/// approval goes back to the child.
#[tokio::test]
async fn test_child_ask_cards_match() {
    let outside = scratch_dir("child");
    create_dir_all(outside.join("workspace")).expect("workspace dir");
    create_dir_all(outside.join("shared")).expect("shared dir");
    let shared_file = outside
        .join("shared")
        .join("report.txt")
        .display()
        .to_string();
    let scenario = Scenario {
        label: "child ask",
        tool: "write",
        input: json!({"path": shared_file}),
        reason: detection("path-bounds", "outside the workspace"),
        fenced: true,
        failing_store: false,
        answer: Answer {
            approved: true,
            scope: ApprovalScope::Workspace,
        },
        escalation: false,
    };
    let message = BusMessage::PermissionRequest {
        child_id: "child-1".into(),
        subagent_type: "explore".into(),
        call_id: "call-1".into(),
        tool: "write".into(),
        input: json!({"path": shared_file}),
        reason: Some(scenario.reason.clone()),
    };

    let (frontend, frontend_verdict) = child_ask_frontend(
        fixture(&scenario, &outside),
        message.clone(),
        scenario.answer,
    )
    .await;
    let (acp, acp_verdict) =
        child_ask_acp(fixture(&scenario, &outside), message, scenario.answer).await;

    assert_eq!(
        acp.options, frontend.options,
        "child ask options differ between transports"
    );
    assert_eq!(
        acp.rules, frontend.rules,
        "child ask installed different rules"
    );
    assert_eq!(acp.dirs, frontend.dirs, "child ask granted dirs differ");
    assert_eq!(acp.audit, frontend.audit, "child ask audits differ");
    assert!(
        acp.dirs.len() == 1 && Path::new(&acp.dirs[0]).ends_with("shared"),
        "the remember answer granted the authorized directory: {:?}",
        acp.dirs
    );
    assert_eq!(
        acp_verdict, frontend_verdict,
        "child verdicts differ (the bus contract carries one scope)"
    );
    assert!(acp_verdict.0, "the child was approved: {acp_verdict:?}");
    remove_dir_all(&outside).ok();
}

async fn child_ask_frontend(
    fixture: Fixture,
    message: BusMessage,
    answer: Answer,
) -> (Outcome, (bool, String)) {
    let bus: Arc<AgentBus> = Arc::new(Default::default());
    let mut verdict_rx = bus.subscribe(&permission_response_topic("child-1", "call-1"));
    let (io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        fixture.runner.clone(),
        fixture.session,
        fixture.gate.clone(),
    )
    .with_bus(Some(bus.clone()));
    let handle = tokio::spawn(async move { server.serve(io).await });
    send_frame(&mut client_tx, &Hello::local()).await;
    drop(recv_line(&mut client_rx).await);
    wait_for_subscriber(&bus).await;
    bus.publish(permission_request_topic(), message);

    let (ask_id, call_id, options) = recv_frontend_ask(&mut client_rx).await;
    send_frame(
        &mut client_tx,
        &ClientFrame::Response(ClientResponseEnvelope::new(
            ask_id,
            ClientResponsePayload::Permission(ApprovalDecision {
                call_id,
                approved: answer.approved,
                updated_input: None,
                scope: answer.scope,
            }),
        )),
    )
    .await;
    let verdict = recv_child_verdict(&mut verdict_rx).await;
    drop(client_tx);
    handle
        .await
        .expect("server task joins")
        .expect("server serves");
    let options: Vec<OptionView> = options.iter().map(OptionView::summarize).collect();
    (collect_outcome(&fixture, options).await, verdict)
}

async fn child_ask_acp(
    fixture: Fixture,
    message: BusMessage,
    answer: Answer,
) -> (Outcome, (bool, String)) {
    let bus: Arc<AgentBus> = Arc::new(Default::default());
    let mut verdict_rx = bus.subscribe(&permission_response_topic("child-1", "call-1"));
    let (client_tx, server_rx) = tokio_mpsc::channel::<String>(256);
    let (server_tx, mut client_rx) = tokio_mpsc::channel::<String>(256);
    let mut io = from_line_channels(server_rx, server_tx);
    let adapter = Arc::new(AcpAdapter::new(
        AcpxCapabilities::default(),
        1,
        SessionLeaseStore::new(),
    ));
    let server = AcpServer::new(
        adapter,
        fixture.runner.clone(),
        fixture.gate.clone(),
        fixture.session,
    )
    .with_bus(Some(bus.clone()));
    let handle = tokio::spawn(async move { server.serve(&mut io).await });
    wait_for_subscriber(&bus).await;
    bus.publish(permission_request_topic(), message);

    let (ask_id, options) = recv_acp_ask(&mut client_rx).await;
    send_line(
        &client_tx,
        &AcpResponse::ok(
            AcpRequestId::Number(ask_id),
            serde_json::to_value(RequestPermissionResponse {
                outcome: RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                    option_id: answer.option_id().into(),
                    meta: None,
                }),
                meta: None,
            })
            .expect("permission response serializes"),
        ),
    )
    .await;
    let verdict = recv_child_verdict(&mut verdict_rx).await;
    drop(client_tx);
    handle
        .await
        .expect("server task joins")
        .expect("server serves");
    let options: Vec<OptionView> = options.iter().map(OptionView::summarize).collect();
    (collect_outcome(&fixture, options).await, verdict)
}

/// Wait until the serve loop holds a subscription to the child ask topic, so
/// a publish cannot land before the server is listening.
async fn wait_for_subscriber(bus: &AgentBus) {
    let topic = permission_request_topic();
    timeout(WAIT, async {
        while bus.receiver_count(topic) == 0 {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("serve loop subscribed to the child ask topic");
}

async fn recv_child_verdict(rx: &mut BroadcastReceiver<BusMessage>) -> (bool, String) {
    let message = timeout(WAIT, rx.recv())
        .await
        .expect("child verdict timeout")
        .expect("child verdict");
    match message {
        BusMessage::PermissionResponse {
            approved, scope, ..
        } => (approved, scope),
        other => panic!("expected a permission response, got {other:?}"),
    }
}

/// A client that closes while a tool runs settles the turn: the paired tool
/// result lands in the durable log before the connection ends, so a later
/// resume sees no orphaned call.
#[tokio::test]
async fn test_acp_disconnect_settles_turn() {
    let entered = Arc::new(Notify::new());
    let (release_tx, release_rx) = oneshot::channel();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HangingTool {
        entered: entered.clone(),
        release: Mutex::new(Some(release_rx)),
    }));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![
        tool_call_response("hanging", json!({})),
        text_response("late"),
    ]));
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let runner = Arc::new(Runner::new(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    ));
    let (client_tx, server_rx) = tokio_mpsc::channel::<String>(256);
    let (server_tx, _client_rx) = tokio_mpsc::channel::<String>(256);
    let mut io = from_line_channels(server_rx, server_tx);
    let adapter = Arc::new(AcpAdapter::new(
        AcpxCapabilities::default(),
        1,
        SessionLeaseStore::new(),
    ));
    let server = AcpServer::new(
        adapter,
        runner.clone(),
        Arc::new(DefaultModeGate::new()),
        session,
    );
    let handle = tokio::spawn(async move { server.serve(&mut io).await });

    send_line(&client_tx, &prompt_request(session)).await;
    timeout(WAIT, entered.notified())
        .await
        .expect("run entered the tool");
    drop(client_tx);
    // The settle guard gets its own budget, so the join waits longer than the
    // turn's five-second close budget before calling it stuck.
    let _served = timeout(WAIT + Duration::from_secs(6), handle)
        .await
        .expect("serve returns after the client left")
        .expect("join");

    let entries = runner.store().replay(session).await.expect("replay");
    let call = entries.iter().any(|entry| {
        matches!(
            &entry.event,
            SessionEvent::ToolCall { call_id, .. } if call_id == "toolu_parity"
        )
    });
    let result = entries.iter().any(|entry| {
        matches!(
            &entry.event,
            SessionEvent::ToolResult { call_id, .. } if call_id == "toolu_parity"
        )
    });
    assert!(call, "the tool call was recorded: {entries:?}");
    assert!(
        result,
        "the settle wrote the paired tool result: {entries:?}"
    );
    drop(release_tx);
}

/// A tool that blocks until the test releases it or the run is cancelled.
struct HangingTool {
    entered: Arc<Notify>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

impl Tool for HangingTool {
    fn name(&self) -> &str {
        "hanging"
    }
    fn description(&self) -> &str {
        "blocks a run until released"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn execute(&self, ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let entered = self.entered.clone();
        let release = self.release.lock().expect("release lock").take();
        Box::pin(async move {
            entered.notify_one();
            let released = async {
                if let Some(receiver) = release {
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

/// A fresh directory per test run, so parallel binaries never share one.
fn scratch_dir(tag: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    temp_dir().join(format!("parity-{tag}-{}-{unique}", id()))
}
