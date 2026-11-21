//! A stdio entry point for the ACP server — the socket surface a stock ACP
//! client drives. The pump reads NDJSON lines from stdin on its own thread
//! and writes each outbound frame to stdout as the server produces it.
//!
//! The runner is a stub provider that returns one canned reply, so a
//! session/prompt completes in a single turn with stopReason end_turn.
//! Real providers plug in behind the same AcpServer.

use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::acp_wire::NewSessionRequest;
use houyicoder_protocol::acpx::AcpxCapabilities;
use houyicoder_provider::FakeProvider;
use houyicoder_service::acp::adapter::AcpAdapter;
use houyicoder_service::acp::pump::spawn_pump;
use houyicoder_service::acp::server::AcpServer;
use houyicoder_service::lifecycle::SessionLeaseStore;
use houyicoder_session::SessionStore;
use std::io::BufReader;
use std::io::stdin;
use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    // The runner drives this one session, and the adapter binding hands its
    // id to session/new, so a stock client learns the target from the reply
    // the protocol defines rather than a process-private stderr convention.
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("hello from stub"));
    let tools = ToolRegistry::new();
    let runner = Arc::new(Runner::new(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    ));
    let runner_for_binding = Arc::clone(&runner);
    let binding_session = session;
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
            })),
    );

    // Drive the server over the real stdio pipe. The pump reads stdin on its
    // own thread and flushes each outbound frame as the server produces it,
    // so a client that waits for a reply before sending the next request
    // never deadlocks.
    let mut pump = spawn_pump(BufReader::new(stdin()), stdout());
    let server = AcpServer::new(adapter, runner, session);
    drop(server.serve(&mut pump).await);
    pump.close().await;
}
