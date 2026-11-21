//! A stdio entry point for the ACP server — the socket surface a stock ACP
//! client drives. The pump reads NDJSON lines from stdin on its own thread
//! and writes each outbound frame to stdout as the server produces it.
//!
//! The runner uses a stub provider by default that returns one canned reply.
//! Setting HOUYICODER_ACP_STUB_HANG makes it stream one delta then wait, so
//! a test can drive the cancel path against a live run.

use std::env::var_os;
use std::future::Future;
use std::io::BufReader;
use std::io::stdin;
use std::io::stdout;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

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
use houyicoder_service::acp::live::AcpLiveStream;
use houyicoder_service::acp::pump::spawn_pump;
use houyicoder_service::acp::server::AcpServer;
use houyicoder_service::lifecycle::SessionLeaseStore;
use houyicoder_session::SessionStore;

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

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    // The runner drives this one session, and the adapter binding hands its
    // id to session/new, so a stock client learns the target from the reply
    // the protocol defines rather than a process-private stderr convention.
    let provider: Arc<dyn ModelProvider> = if var_os("HOUYICODER_ACP_STUB_HANG").is_some() {
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
    // Install the live sink before the runner is shared so each response
    // delta reaches the serve loop as a session/update chunk.
    let (sink, live) = AcpLiveStream::channel();
    sink.install_on(&mut runner);
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
                // The example accepts the client's cwd when it is absolute;
                // a relative one keeps the startup directory, logged rather
                // than fatal so the demo stays usable.
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
        .with_live_stream(live)
        .with_model_options(models);
    drop(server.serve(&mut pump).await);
    pump.close().await;
}
