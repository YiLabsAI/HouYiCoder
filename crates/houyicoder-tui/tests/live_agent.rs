//! Live integration test: drives the real agent loop against the real LLM
//! (DashScope qwen3.7-max via .env). Ignored by default so make check never
//! hits the network. Run the ignored live-agent integration test with .env
//! sourced (see the Makefile / CI for the exact cargo invocation).

#![allow(clippy::unwrap_in_result)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use houyicoder_api::provider::ModelProvider;
use houyicoder_client::{Client, InProcTransport};
use houyicoder_core::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{RunOutcome, Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_provider::OpenAiCompatibleProvider;
use houyicoder_service::server::{Server, ServerIo};
use houyicoder_session::SessionStore;
use houyicoder_tui::state::TranscriptLine;

/// Pair an in-memory server + client around a runner, mirroring the CLI
/// composition root: install the live delta sink (streams acpx/llm/* deltas
/// onto the wire), Arc the runner, spawn the server, return the shared
/// runner + un-connected client.
fn pair_inproc(
    mut runner: Runner,
    session: SessionId,
    gate: Arc<houyicoder_permission::DefaultModeGate>,
    _agent_tx: std::sync::mpsc::Sender<houyicoder_tui::run_control::AgentMessage>,
) -> (Arc<Runner>, Client) {
    let (c2s_tx, c2s_rx) = futures::channel::mpsc::channel(16);
    let (s2c_tx, s2c_rx) = futures::channel::mpsc::channel(16);
    let next_seq = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    houyicoder_service::server::install_live_sink(&mut runner, s2c_tx.clone(), next_seq.clone());
    let runner = Arc::new(runner);
    let server_io = ServerIo::new(s2c_tx, c2s_rx);
    // The server takes the composition's gate so wire mode/rule writes reach
    // the gate the GuardedTool wrappers actually use.
    let gate_dyn: Arc<dyn houyicoder_permission::ModeGate> = gate;
    let server = Server::new_with_shared_seq(runner.clone(), session, gate_dyn, next_seq);
    let runtime = houyicoder_tui::composition::shared_runtime();
    runtime.spawn(async move {
        let _serve = server.serve(server_io).await;
    });
    let transport = InProcTransport::from_halves(c2s_tx, s2c_rx);
    (runner, Client::new(Box::new(transport)))
}

/// Poll the app's agent channel until the in-flight run lands (agent_busy
/// flips false) or the timeout expires. Returns true when a message landed.
fn drain(app: &mut houyicoder_tui::state::App, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        app.poll_agent();
        if !app.agent_busy {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Wire an app whose runner is backed by the real .env provider and the real
/// mac sandbox + bash/read/write tools, so a live model can actually call tools.
/// When manual is true the gate starts in Manual mode so destructive tools
/// (bash) raise an approval card instead of auto-running — the
/// live_bash_tool_roundtrip spine needs that HITL pause; the chat-reply test
/// stays Auto (no tool call, no approval).
fn live_app_with_tools(manual: bool) -> houyicoder_tui::state::App {
    let bundle = houyicoder_service::composition::build_runner(
        houyicoder_service::composition::BuildRunnerOptions::default(),
    );
    let runner = bundle.runner;
    let session = bundle.session;
    let gate = bundle.gate;
    if manual {
        // set_mode is on the ModeGate trait (&self), and the same Arc is
        // shared by the server + the runner's GuardedTool wrappers, so one
        // call flips both — before pair_inproc moves the gate into the server.
        houyicoder_permission::ModeGate::set_mode(
            &*gate,
            houyicoder_permission::PermissionMode::Manual,
            "live_bash_tool_roundtrip needs HITL approval",
        );
    }
    let wire_session = houyicoder_protocol::frontend::SessionId(session.to_string());
    let (tx, rx) = std::sync::mpsc::channel::<houyicoder_tui::run_control::AgentMessage>();
    let (runner, client) = pair_inproc(runner, session, gate, tx.clone());
    drop(runner); // server owns the runner; the TUI holds no engine handle.
    houyicoder_tui::composition::build_app(houyicoder_tui::composition::RunnerBundle {
        client,
        agent_tx: tx,
        agent_rx: rx,
        session: wire_session,
        model: houyicoder_config::resolve_model(),
        trajectory_log: None,
        export_log: None,
        snapshot: None,
        session_lister: None,
        skip_login: false,
        startup_warnings: Vec::new(),
    })
}

#[test]
#[ignore]
fn test_live_bash_tool_roundtrip() {
    // Proves the full spine with a real model: qwen3.7-max must emit a bash
    // tool_call our provider parses, the runner raises Interruption for HITL,
    // approval + resume drives the sandbox, and the model reports the result.
    let mut app = live_app_with_tools(true);
    assert!(app.session.is_some(), "carrier must be wired");
    app.spawn_run(
        "Use the bash tool to run the command: echo hello-from-tool. \
         Then reply with exactly the text it printed."
            .into(),
    );
    // Reasoning models can take a while on the first turn.
    assert!(
        drain(&mut app, 90_000),
        "agent did not produce a tool call within 90s"
    );
    assert!(
        app.approval.is_some(),
        "expected an approval popup (model should call bash), got: {:?}",
        app.transcript
            .iter()
            .map(|l| l.render())
            .collect::<Vec<_>>()
    );
    let tool = app.approval.as_ref().unwrap().tool.clone();
    assert!(
        tool.contains("bash") || tool == "bash",
        "approval tool should be bash, got {tool}"
    );
    // Approve the current approval (one decision for its call_id) and resume;
    // the sandbox runs, the result feeds back, the model emits a final answer.
    let call_id = app.approval.as_ref().unwrap().call_id.clone();
    app.resolve_current_approval(houyicoder_protocol::frontend::run::ApprovalDecision {
        call_id,
        approved: true,
        updated_input: None,
        scope: "once".to_string(),
    });
    assert!(
        drain(&mut app, 90_000),
        "agent did not finish after resume within 90s"
    );
    let transcript = app
        .transcript
        .iter()
        .map(|l| l.render())
        .collect::<Vec<_>>()
        .join("\n");
    println!("{transcript}");
    assert!(
        transcript.contains("hello-from-tool"),
        "transcript should carry the echoed tool output: {transcript}"
    );
}

#[test]
#[ignore]
fn test_chat_replies_to_greeting() {
    if std::env::var("DASHSCOPE_API_KEY").is_err() {
        eprintln!("skip: DASHSCOPE_API_KEY not set");
        return;
    }
    let mut app = live_app_with_tools(false);
    assert!(app.session.is_some(), "carrier must be wired");
    app.spawn_run("say hello in one short sentence".into());
    assert!(drain(&mut app, 30_000), "agent did not reply within 30s");
    let has_reply = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::Agent(s) if !s.is_empty()));
    assert!(has_reply, "transcript should carry a real agent reply");
    let is_stub = app
        .transcript
        .iter()
        .any(|l| matches!(l, TranscriptLine::Agent(s) if s.contains("stub mode")));
    assert!(!is_stub, "should not be the stub fallback reply");
    println!(
        "transcript rows: {}",
        app.transcript
            .iter()
            .map(|l| l.render())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
#[ignore]
fn test_live_provider_returns_text() {
    if std::env::var("DASHSCOPE_API_KEY").is_err() {
        eprintln!("skip: DASHSCOPE_API_KEY not set");
        return;
    }
    let key = std::env::var("DASHSCOPE_API_KEY").unwrap();
    let base = std::env::var("DASHSCOPE_BASE_URL").unwrap_or_default();
    let model = houyicoder_config::resolve_model();
    let provider: Arc<dyn ModelProvider> = Arc::new(OpenAiCompatibleProvider::new(base, key));
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Arc::new(Runner::with_shared_store(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model,
            instructions: "you are a test agent".into(),
            max_turns: 3,
            ..RunnerConfig::default()
        },
    ));
    let session = SessionId::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = rt
        .block_on(async move { runner.run(session, "say hi".into()).await })
        .expect("run succeeds");
    match result.outcome {
        RunOutcome::FinalOutput(t) => {
            assert!(!t.is_empty(), "reply text should be non-empty");
            println!("real LLM reply: {t}");
        }
        other => panic!("expected FinalOutput, got {other:?}"),
    }
}
