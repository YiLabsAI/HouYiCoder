//! The ACP processes the cross-layer tests drive as children.
//!
//! serve_stdio_server is this crate's own northbound ACP server over stdio:
//! a stock client can drive it, and the southbound client can delegate to it,
//! so the tests exercise the real wire on both sides. serve_child_stub is a
//! minimal child that reaches paths the server's canned provider cannot: a
//! reverse permission ask, an ask aimed outside the child's root, and a turn
//! that never ends.
//!
//! Both are entered by the test binary itself, so no nested build is needed:
//! the parent spawns current_exe() with the mode variable set and speaks to
//! its pipes.

use std::env::var_os;
use std::fs::write;
use std::future::Future;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::io::stdin;
use std::io::stdout;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use futures::Stream;
use futures::StreamExt;
use futures::stream::iter;
use futures::stream::pending;
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_permission::ModeGate;
use houyicoder_protocol::acp_wire::NewSessionRequest;
use houyicoder_protocol::acpx::AcpxCapabilities;
use houyicoder_protocol::llm::CompletionRequest;
use houyicoder_protocol::llm::CompletionResponse;
use houyicoder_protocol::llm::LlmEvent;
use houyicoder_protocol::llm::ModelCapabilities;
use houyicoder_protocol::llm::ProviderError;
use houyicoder_protocol::llm::Usage;
use houyicoder_provider::FakeProvider;
use houyicoder_service::acp::adapter::AcpAdapter;
use houyicoder_service::acp::adapter::SessionFacts;
use houyicoder_service::acp::adapter::SessionView;
use houyicoder_service::acp::adapter::model_options;
use houyicoder_service::acp::adapter::session_config_options;
use houyicoder_service::acp::adapter::session_modes;
use houyicoder_service::acp::pump::spawn_pump;
use houyicoder_service::acp::server::AcpServer;
use houyicoder_service::lifecycle::SessionLeaseStore;
use houyicoder_service::turn_events::TurnEvents;
use houyicoder_session::SessionStore;
use serde_json::Value;
use serde_json::json;
use tokio::runtime::Builder as RuntimeBuilder;

/// The environment variable that turns a test process into the ACP server.
#[allow(dead_code)] // not every binary that includes this module serves both child roles
pub const SERVER_ENV: &str = "HOUYICODER_ACP_CHILD_SERVER";
/// The argv switch that turns a test process into the stub child. The mode
/// switches follow it: --ask sends one reverse permission ask, --escape aims
/// that ask outside the child's root, --hang never ends the turn, and --touch
/// names a file the child writes after it streams so a test can latch on the
/// child having spoken instead of sleeping.
#[allow(dead_code)] // not every binary that includes this module serves both child roles
pub const STUB_ARG: &str = "--stub";
/// The environment switch that makes the served run stream one delta and
/// wait, so a cancel can be driven against a live run.
#[allow(dead_code)] // not every binary that includes this module serves both child roles
pub const SERVER_HANG_ENV: &str = "HOUYICODER_ACP_STUB_HANG";

/// A provider that streams one text delta then never yields again, so a run
/// is live and a cancel can be observed against it.
struct HangingProvider {
    events: Vec<LlmEvent>,
}

impl HangingProvider {
    fn new(events: Vec<LlmEvent>) -> Self {
        Self { events }
    }
}

impl ModelProvider for HangingProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<CompletionResponse, ProviderError>> + Send + '_>> {
        Box::pin(async {
            Ok(CompletionResponse {
                output: vec![],
                usage: Usage::default(),
                model: "test".into(),
            })
        })
    }
    fn stream(
        &self,
        _req: CompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<LlmEvent, ProviderError>> + Send + '_>> {
        let prefix = iter(self.events.clone().into_iter().map(Ok));
        let tail = pending();
        Box::pin(prefix.chain(tail))
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// Serve the ACP protocol over this process's stdio until the client closes
/// its end. The runner behind it answers with one canned reply, or streams a
/// single delta and waits when the hang switch is set.
#[allow(dead_code)] // not every binary that includes this module serves both child roles
pub fn serve_stdio_server() {
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");
    runtime.block_on(serve_inner());
}

async fn serve_inner() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    // The runner drives this one session, and the adapter binding hands its
    // id to session/new, so a stock client learns the target from the reply
    // the protocol defines rather than a process-private stderr convention.
    let provider: Arc<dyn ModelProvider> = if var_os(SERVER_HANG_ENV).is_some() {
        Arc::new(HangingProvider::new(vec![LlmEvent::TextDelta {
            id: "t1".into(),
            text: "partial".into(),
        }]))
    } else {
        Arc::new(FakeProvider::text("hello from stub"))
    };
    let tools = ToolRegistry::new();
    let mut runner = Runner::new(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    let models = model_options(&runner);
    // Route the live events before the runner is shared so response deltas,
    // tool progress, and user notices all reach the serve loop.
    let events = TurnEvents::new();
    events.install_on(&mut runner);
    let stream = events.subscribe();
    let runner = Arc::new(runner);
    let runner_for_binding = Arc::clone(&runner);
    let binding_session = session;
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let facts_runner = Arc::clone(&runner);
    let facts_gate = Arc::clone(&gate);
    let facts_models = models.clone();
    let facts: SessionFacts = Arc::new(move || SessionView {
        modes: session_modes(facts_gate.as_ref()),
        config_options: session_config_options(facts_runner.as_ref(), &facts_models),
    });
    let adapter = Arc::new(
        AcpAdapter::new(AcpxCapabilities::default(), 1, SessionLeaseStore::new())
            .with_session_binding(Arc::new(move |params: &NewSessionRequest| {
                // The server accepts the client's cwd when it is absolute; a
                // relative one keeps the startup directory, logged rather
                // than fatal.
                let cwd = PathBuf::from(&params.cwd);
                if cwd.is_absolute() {
                    runner_for_binding.switch_cwd(cwd);
                } else {
                    tracing::warn!(cwd = %params.cwd, "session/new cwd is not absolute");
                }
                binding_session
            }))
            .with_session_facts(facts),
    );

    // Drive the server over the real stdio pipe. The pump reads stdin on its
    // own thread and flushes each outbound frame as the server produces it,
    // so a client that waits for a reply before sending the next request
    // never deadlocks.
    let mut pump = spawn_pump(BufReader::new(stdin()), stdout());
    let server = AcpServer::new(adapter, runner, gate, session)
        .with_events(stream)
        .with_model_options(models);
    drop(server.serve(&mut pump).await);
    pump.close().await;
}

/// Serve as a minimal ACP child: answer the handshake, stream one chunk, and
/// end the turn. The switches reach a reverse permission ask, an ask outside
/// the child's root, a turn that never ends, and a marker file the test
/// latches on.
#[allow(dead_code)] // not every binary that includes this module serves both child roles
pub fn serve_child_stub() {
    let mut ask = false;
    let mut escape = false;
    let mut hang = false;
    let mut touch: Option<PathBuf> = None;
    let mut args = std::env::args().skip_while(|arg| arg != STUB_ARG).skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ask" => ask = true,
            "--escape" => escape = true,
            "--hang" => hang = true,
            "--touch" => touch = args.next().map(PathBuf::from),
            _ => {}
        }
    }
    let mut session_id = String::from("01JSTUBACPCHILD01");
    let mut next_id = 100i64;
    let mut waiting: Option<Waiting> = None;
    for line in BufReader::new(stdin()).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = frame.get("id").and_then(Value::as_i64);
        if let Some(ask_id) = waiting.as_ref().map(|w| w.ask_id)
            && id == Some(ask_id)
        {
            let verdict = frame
                .get("result")
                .and_then(|r| r.get("outcome"))
                .map(|o| o.to_string())
                .unwrap_or_else(|| "no-outcome".to_string());
            chunk(&format!(" verdict={verdict}"));
            if let Some(w) = waiting.take() {
                reply(Some(w.prompt_id), json!({"stopReason": "end_turn"}));
            }
            continue;
        }
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        match method {
            "initialize" => reply(id, json!({"protocolVersion": 1})),
            "session/new" => {
                if let Some(cwd) = frame
                    .get("params")
                    .and_then(|p| p.get("cwd"))
                    .and_then(Value::as_str)
                {
                    session_id = format!("01JSTUBACPCHILD01-{}", short_hash(cwd));
                }
                reply(id, json!({"sessionId": session_id}));
            }
            "session/prompt" => {
                let Some(prompt_id) = id else { continue };
                chunk("stub child answer");
                if let Some(path) = &touch {
                    drop(write(path, b"spoke"));
                }
                if hang {
                    sleep(Duration::from_secs(30));
                }
                if ask {
                    let ask_id = next_id;
                    next_id += 1;
                    send_ask(ask_id, &session_id, escape);
                    waiting = Some(Waiting { prompt_id, ask_id });
                } else {
                    reply(Some(prompt_id), json!({"stopReason": "end_turn"}));
                }
            }
            "session/cancel" => {}
            _ => {}
        }
    }
}

/// What the stub still owes: the prompt awaiting its answer, and the
/// permission ask whose verdict it awaits before answering that prompt.
struct Waiting {
    prompt_id: i64,
    ask_id: i64,
}

/// A stable short suffix so two children in one test run are distinguishable
/// without depending on the random session id space.
fn short_hash(text: &str) -> String {
    let mut hash: u64 = 1469598103934665603;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    format!("{:08x}", hash as u32)
}

fn send_ask(request_id: i64, session_id: &str, escape: bool) {
    let location = if escape {
        "/tmp/outside-the-child-root.txt"
    } else {
        "inside.txt"
    };
    write_line(&json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/request_permission",
        "params": {
            "sessionId": session_id,
            "toolCall": {
                "toolCallId": "call-1",
                "title": "write_file",
                "locations": [{"path": location}],
                "rawInput": {"tool_name": "write_file"}
            },
            "options": [
                {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "reject_once", "name": "Reject once", "kind": "reject_once"}
            ]
        }
    }));
}

fn chunk(text: &str) {
    notify(json!({
        "sessionUpdate": "agent_message_chunk",
        "content": {"type": "text", "text": text}
    }));
}

fn reply(id: Option<i64>, result: Value) {
    let Some(id) = id else { return };
    write_line(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn notify(update: Value) {
    write_line(&json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"update": update}
    }));
}

fn write_line(value: &Value) {
    let mut out = stdout();
    // A closed pipe means the parent is gone; the loop above ends on its own
    // read error, so an unflushed line has nobody left to reach.
    if writeln!(out, "{value}").is_ok() {
        drop(out.flush());
    }
}
