//! Cross-layer proof that the parent host delegates work to an external ACP
//! child running as a real process: handshake, a streamed turn folded into
//! the child log, reverse permission asks answered through the parent fence
//! and gate, a cancel that reaps the child, and a refused spawn for a
//! missing program.
//!
//! This binary is its own child. It runs harness-free, so the child's stdout
//! carries only protocol frames, and the parent reaches it through
//! current_exe with an argv mode switch: --server serves this crate's own
//! northbound server, --stub serves the minimal child that reaches the paths
//! the server's canned provider cannot.

#[path = "common/acp_agent.rs"]
mod acp_agent;
mod common;

use std::env::current_exe;
use std::env::vars;
use std::fs::read_dir;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use houyicoder_api::launcher::{ProcessLauncher, StdProcessLauncher};
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_api::spawn::{SpawnArgs, SpawnFailure, SpawnHandle};
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::CancellationToken;
use houyicoder_async::bus::MessageBus;
use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, BusMessage, permission_request_topic, permission_response_topic,
};
use houyicoder_core::agent::multi_agent::registry::{
    AgentBackend, AgentDefinition, AgentRegistry, BuiltInRegistry, ExternalAcpSpec, PromptSource,
};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::worktree_controller::WorktreeController;
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::{DefaultModeGate, ModeGate, PermissionMode};
use houyicoder_platform::process::alive as process_alive;
use houyicoder_provider::FakeProvider;
use houyicoder_service::acp::client::session::{AcpChildSession, PermissionFence};
use houyicoder_service::composition::multi_agent::{MultiAgentDeps, MultiAgentRuntime};
use houyicoder_session::SessionStore;
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::time::sleep;
use tokio::time::timeout;

use crate::common::{StubSandbox, make_git_repo};

/// The definition whose backend is this binary in one child mode.
fn external_definition(args: Vec<String>) -> AgentDefinition {
    let program = current_exe().expect("the test binary path");
    AgentDefinition {
        subagent_type: "review".into(),
        when_to_use: "external reviewer".into(),
        tools: None,
        disallowed_tools: vec![],
        model: None,
        effort: None,
        permission_mode: None,
        max_turns: None,
        skills: vec![],
        mcp_servers: vec![],
        hooks: vec![],
        initial_prompt: None,
        memory: Default::default(),
        isolation: Default::default(),
        omit_project_context: false,
        color: None,
        system_prompt: PromptSource::Owned("review".into()),
        backend: AgentBackend::ExternalAcp(ExternalAcpSpec {
            program: program.display().to_string(),
            args,
            passthrough_env: vec![],
            env: Vec::new(),
            handshake_timeout_ms: 10_000,
        }),
    }
}

fn stub_args(mode: &[&str]) -> Vec<String> {
    let mut args = vec![acp_agent::STUB_ARG.to_string()];
    args.extend(mode.iter().map(|s| s.to_string()));
    args
}

struct Harness {
    runtime: MultiAgentRuntime,
    store: Arc<SessionStore>,
}

fn runtime(
    definition: AgentDefinition,
    workspace: PathBuf,
    bus: Option<Arc<AgentBus>>,
    gate: Option<Arc<dyn ModeGate>>,
    worktree: Option<Arc<WorktreeController>>,
) -> Harness {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(vec![definition]));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("unused"));
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store: store.clone(),
        provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig::default(),
        worktree_controller: worktree,
        workspace: Some(workspace),
        bus,
        descriptor_store: None,
        skill_registry: None,
        conditional: None,
        hooks: None,
        launcher: Arc::new(StdProcessLauncher::new()) as Arc<dyn ProcessLauncher>,
        mode_gate: gate,
    });
    Harness { runtime, store }
}

/// Every assistant text the child's session recorded, joined.
fn child_text(store: &SessionStore, child_sid: &str) -> String {
    let sid = parse_session(child_sid);
    store
        .trajectory_tail(sid)
        .into_iter()
        .filter_map(|entry| match entry.event {
            SessionEvent::AssistantMessage { text, .. } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn parse_session(display: &str) -> SessionId {
    SessionId::from_display_string(display).expect("a session id this host minted")
}

/// The host drives this crate's own ACP server as an external child: the
/// handshake binds a child session, the streamed reply is folded into that
/// child's log, and the worktree the call asked for is gone afterwards.
async fn test_acp_child_round_trip() {
    let repo = make_git_repo("acp-round-trip");
    let log: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let controller = Arc::new(WorktreeController::new(
        repo.clone(),
        repo.join(".git"),
        Arc::new(StubSandbox),
        log,
        SessionId::new(),
    ));
    let harness = runtime(
        external_definition(vec!["--server".to_string()]),
        repo.clone(),
        None,
        None,
        Some(controller),
    );
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    let mut args = SpawnArgs::new("review", "review the diff", "review");
    args.isolation = "worktree".into();
    let outcome = harness.runtime.spawn(&ctx, args).await.expect("spawn");
    assert_eq!(outcome.status.as_deref(), Some("completed"));
    assert!(
        outcome.usage.is_none(),
        "an external child reports no usage"
    );
    let text = child_text(&harness.store, &outcome.child_session_id);
    assert!(
        text.contains("hello from stub"),
        "the child's streamed reply is folded into its own log: {text:?}"
    );
    let leftovers = read_dir(repo.join(".houyicoder").join("worktrees"))
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "the child worktree was reclaimed");
}

/// A child asking to write outside its own root is refused without the
/// parent user, and the refusal reaches the child.
async fn test_acp_child_escape_rejected() {
    let root = make_git_repo("acp-escape");
    let harness = runtime(
        external_definition(stub_args(&["--ask", "--escape"])),
        root,
        None,
        Some(Arc::new(DefaultModeGate::new())),
        None,
    );
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    let args = SpawnArgs::new("review", "write outside", "escape");
    let outcome = harness.runtime.spawn(&ctx, args).await.expect("spawn");
    assert_eq!(outcome.status.as_deref(), Some("completed"));
    let text = child_text(&harness.store, &outcome.child_session_id);
    assert!(
        text.contains("reject_once"),
        "the child learns the ask was refused: {text:?}"
    );
}

/// A child asking for a call inside its own root reaches the parent user; the
/// approval travels back and the child continues.
async fn test_child_ask_reaches_parent() {
    let root = make_git_repo("acp-ask");
    let bus: Arc<AgentBus> = Arc::new(Default::default());
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::with_mode(PermissionMode::Manual));
    let harness = runtime(
        external_definition(stub_args(&["--ask"])),
        root,
        Some(bus.clone()),
        Some(gate),
        None,
    );
    // Subscribed before the spawn, so the ask cannot land before the listener.
    let mut asks = bus.subscribe(permission_request_topic());
    // The parent's own table is read at the moment the ask arrives: a host
    // that holds a spawn handle verifies against it, so an ask missing here
    // is one the real host would discard as stale.
    let pending = harness.runtime.pending_child_permissions();
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    let args = SpawnArgs::new("review", "write inside", "ask");
    let handle = tokio::spawn(async move { harness.runtime.spawn(&ctx, args).await });
    let ask = timeout(Duration::from_secs(20), asks.recv())
        .await
        .expect("the ask reaches the parent within the budget")
        .expect("an ask is published");
    let BusMessage::PermissionRequest {
        child_id, call_id, ..
    } = ask
    else {
        panic!("expected a permission request, got {ask:?}");
    };
    assert!(
        pending
            .lock()
            .expect("the table locks")
            .iter()
            .any(|p| p.call_id == call_id),
        "the ask is parked before it is published, or the host would drop it"
    );
    bus.publish(
        &permission_response_topic(&child_id, &call_id),
        BusMessage::PermissionResponse {
            call_id: call_id.clone(),
            approved: true,
            updated_input: None,
            scope: "once".into(),
        },
    );
    let outcome = timeout(Duration::from_secs(20), handle)
        .await
        .expect("the child finishes once answered")
        .expect("spawn task joined")
        .expect("spawn succeeds");
    assert_eq!(outcome.status.as_deref(), Some("completed"));
    assert!(
        pending.lock().expect("the table locks").is_empty(),
        "the answered ask leaves nothing parked"
    );
}

/// A cancel during a live turn stops the child and reaps its process.
async fn test_acp_child_kill_reaps() {
    let root = make_git_repo("acp-kill");
    let spoke = root.join("spoke.marker");
    let spec = ExternalAcpSpec {
        program: current_exe()
            .expect("the test binary path")
            .display()
            .to_string(),
        args: stub_args(&["--hang", "--touch", &spoke.display().to_string()]),
        passthrough_env: vec![],
        env: Vec::new(),
        handshake_timeout_ms: 10_000,
    };
    let launcher: Arc<dyn ProcessLauncher> = Arc::new(StdProcessLauncher::new());
    let host_env: Vec<(String, String)> = vars().collect();
    let mut session =
        AcpChildSession::connect(&launcher, &spec, &root, &host_env).expect("connect");
    let child_sid = session.handshake(&spec, &root).await.expect("handshake");
    let pid = session.process_id().expect("a real child has a pid");
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let log: Arc<dyn SessionLog> = store.clone();
    let cancel = CancellationToken::new();
    let cancel_for_task = cancel.clone();
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let task = tokio::spawn(async move {
        let fence = PermissionFence {
            workspace_root: root.clone(),
            gate: Some(gate),
            bus: None,
            pending: Arc::new(Mutex::new(Vec::new())),
            cancel: cancel_for_task.clone(),
            child_id: child_sid.to_string(),
            agent_type: "review".into(),
        };
        session
            .drive_prompt(child_sid, "hang", &log, &cancel_for_task, Some(&fence))
            .await
    });
    // Latched on the child writing its marker, so the cancel lands on a live
    // turn rather than racing its first frame.
    let mut saw_chunk = false;
    for _ in 0..200 {
        if spoke.exists() {
            saw_chunk = true;
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    assert!(saw_chunk, "the child streamed before the cancel");
    cancel.cancel();
    let err = timeout(Duration::from_secs(10), task)
        .await
        .expect("the cancel path returns inside its budget")
        .expect("drive task joined")
        .expect_err("a cancelled child does not end normally");
    assert!(format!("{err}").contains("cancel"), "{err}");
    // The session is gone with the task; its drop is the launcher's kill path.
    // Poll the platform's own liveness probe rather than assuming the drop
    // ran and the kernel finished reaping in the same instant. The probe reads
    // the process table, so it answers on every platform this suite builds.
    let pid = i32::try_from(pid).expect("the pid fits an i32");
    let mut alive = true;
    for _ in 0..100 {
        alive = process_alive(pid);
        if !alive {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive, "the child process {pid} was reaped");
}

/// A definition naming a program the launcher cannot start is refused, and the
/// failure names the program.
async fn test_acp_child_missing_program() {
    let root = make_git_repo("acp-missing");
    let mut definition = external_definition(vec![]);
    definition.backend = AgentBackend::ExternalAcp(ExternalAcpSpec {
        program: "no-such-acp-program-xyz".into(),
        args: vec![],
        passthrough_env: vec![],
        env: Vec::new(),
        handshake_timeout_ms: 2_000,
    });
    let harness = runtime(definition, root, None, None, None);
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    let args = SpawnArgs::new("review", "review", "missing");
    let err = harness
        .runtime
        .spawn(&ctx, args)
        .await
        .expect_err("must fail");
    match err {
        SpawnFailure::ExternalFailed { reason } => {
            assert!(reason.contains("no-such-acp-program-xyz"), "{reason}");
        }
        other => panic!("expected ExternalFailed, got {other:?}"),
    }
}

/// The tests, plus the two child modes this binary serves for them.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--server") {
        acp_agent::serve_stdio_server();
        return;
    }
    if args.iter().any(|arg| arg == acp_agent::STUB_ARG) {
        acp_agent::serve_child_stub();
        return;
    }
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");
    let boxed = |f: fn() -> common::BoxTestFuture| f;
    common::run_async_tests(
        &runtime,
        &[
            (
                "test_acp_child_round_trip",
                boxed(|| Box::pin(test_acp_child_round_trip())),
            ),
            (
                "test_acp_child_escape_rejected",
                boxed(|| Box::pin(test_acp_child_escape_rejected())),
            ),
            (
                "test_child_ask_reaches_parent",
                boxed(|| Box::pin(test_child_ask_reaches_parent())),
            ),
            (
                "test_acp_child_kill_reaps",
                boxed(|| Box::pin(test_acp_child_kill_reaps())),
            ),
            (
                "test_acp_child_missing_program",
                boxed(|| Box::pin(test_acp_child_missing_program())),
            ),
        ],
    );
}
