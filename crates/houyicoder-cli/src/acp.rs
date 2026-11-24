//! The ACP stdio entry: the surface a stock ACP client drives. The handshake
//! is answered before a session exists; the workspace the client names in
//! session/new decides the session log, the sandbox, and the file tools, so
//! the runner is built once that request arrives.

use std::env;
use std::error::Error;
use std::io::BufReader;
use std::io::stdin;
use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;

use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::multi_agent::bus_types::AgentBus;
use houyicoder_permission::ModeGate;
use houyicoder_protocol::acp_wire::AcpErrorCode;
use houyicoder_protocol::acp_wire::AcpRequest;
use houyicoder_protocol::acp_wire::AcpResponse;
use houyicoder_protocol::acp_wire::NewSessionRequest;
use houyicoder_protocol::acpx::AcpxCapabilities;
use houyicoder_protocol::framing::encode;
use houyicoder_service::acp::adapter::AcpAdapter;
use houyicoder_service::acp::adapter::SessionFacts;
use houyicoder_service::acp::adapter::SessionView;
use houyicoder_service::acp::adapter::session_config_options;
use houyicoder_service::acp::adapter::session_modes;
use houyicoder_service::acp::pump::AcpDuplexPump;
use houyicoder_service::acp::pump::InboundFrame;
use houyicoder_service::acp::pump::spawn_pump;
use houyicoder_service::acp::server::AcpServer;
use houyicoder_service::composition::background_tasks::BackgroundTasks;
use houyicoder_service::composition::build_runner;
use houyicoder_service::composition::resolve_provider;
use houyicoder_service::composition::session_log_root;
use houyicoder_service::lifecycle::SessionLeaseStore;
use houyicoder_service::mcp::McpConnections;
use houyicoder_service::turn_events::TurnEvents;
use tokio::runtime::Builder;
use tokio::runtime::Handle;

use crate::production_runner;
use crate::session::housekeeping::fire_after_bundle;
use crate::session::lock::SessionLock;
use crate::session::lock::StoreIdentity;

/// Run the ACP server over stdin and stdout until the client closes the
/// stream.
pub(crate) fn run_acp_stdio(
    project: Option<String>,
    model_override: Option<String>,
) -> Result<(), Box<dyn Error>> {
    // The server runs on a multi-thread runtime so the real provider (HTTP
    // calls) and the sandbox (subprocess spawns) have a runtime that can
    // handle blocking work and child reaping.
    let runtime = Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let mut pump = spawn_pump(BufReader::new(stdin()), stdout());
        // The handshake is answered before any session exists. Its one
        // significant frame is session/new: the workspace it names decides
        // the fence, the tools, and the session log, so the runner is built
        // once that request arrives rather than from the launch directory.
        let handshake = AcpAdapter::new(AcpxCapabilities::default(), 1, SessionLeaseStore::new());
        loop {
            let Some(frame) = pump.next_frame().await else {
                pump.close().await;
                return Ok(());
            };
            let request = match frame {
                InboundFrame::Request(request) if request.method == "session/new" => request,
                InboundFrame::Request(request) => {
                    let response = handshake.handle(&request).await;
                    send_acp_frame(&mut pump, &response).await?;
                    continue;
                }
                _ => continue,
            };
            let workspace = match workspace_for(&request, project.as_deref()) {
                Ok(workspace) => workspace,
                Err(message) => {
                    let response =
                        AcpResponse::err(request.id.clone(), AcpErrorCode::InvalidParams, message);
                    send_acp_frame(&mut pump, &response).await?;
                    continue;
                }
            };
            let parts = build_acp_session(workspace, model_override.as_deref())?;
            // Answer session/new through the same binding the server will
            // use, so the id and modes the client receives are the ones the
            // running session drives.
            let reply = parts.adapter.handle(&request).await;
            send_acp_frame(&mut pump, &reply).await?;
            fire_after_bundle(parts.session);
            let server = AcpServer::new(parts.adapter, parts.runner, parts.gate, parts.session)
                .with_events(parts.events.subscribe())
                .with_model_options(parts.model_options)
                .with_bus(parts.bus);
            drop(server.serve(&mut pump).await);
            parts.background_tasks.shutdown();
            // The session is over, so its external tool servers end here
            // rather than at process exit.
            if let Some(manager) = parts.mcp {
                manager.shutdown().await;
            }
            // serve returns when the client closes stdin; close waits for the
            // writer to flush the last reply before the process exits.
            pump.close().await;
            return Ok(());
        }
    })
}

/// The workspace a session/new request names, or the launch directory when
/// the request carries none. A relative path is not a workspace a fence can
/// pin to, so it falls back the same way.
fn workspace_for(request: &AcpRequest, project: Option<&str>) -> Result<Option<String>, String> {
    let params: NewSessionRequest = request
        .params
        .as_ref()
        .ok_or_else(|| "session/new carries no params".to_string())
        .and_then(|value| serde_json::from_value(value.clone()).map_err(|e| e.to_string()))?;
    if PathBuf::from(&params.cwd).is_absolute() {
        return Ok(Some(params.cwd));
    }
    tracing::warn!(cwd = %params.cwd, "session/new cwd is not absolute; keeping the launch workspace");
    Ok(project.map(str::to_string).or_else(|| {
        env::current_dir()
            .ok()
            .map(|dir| dir.to_string_lossy().into_owned())
    }))
}

/// The pieces one ACP session needs, built for the workspace the client named.
struct AcpSessionParts {
    adapter: Arc<AcpAdapter>,
    runner: Arc<Runner>,
    gate: Arc<dyn ModeGate>,
    session: SessionId,
    events: TurnEvents,
    model_options: Vec<String>,
    background_tasks: Arc<BackgroundTasks>,
    bus: Option<Arc<AgentBus>>,
    /// The session's external tool servers, ended when the client closes.
    mcp: Option<Arc<McpConnections>>,
}

/// Build the runner, fences, and adapter for one workspace. The session log,
/// the sandbox, and the file tools are all rooted at this directory.
fn build_acp_session(
    workspace: Option<String>,
    model_override: Option<&str>,
) -> Result<AcpSessionParts, Box<dyn Error>> {
    let provider = resolve_provider(workspace.as_deref());
    let mut options = production_runner(workspace, provider);
    // This entry assembles inside its own runtime's block_on, so the tool
    // servers start on that runtime rather than on a second pool.
    options.runtime = Handle::try_current().ok();
    let mut bundle = build_runner(options);
    if let Some(model) = model_override {
        bundle.runner.set_model(model.to_string());
    }
    // The ACP server records this session for as long as it runs, so it holds
    // the session lock, as the detached server does.
    let lease = SessionLock::acquire(&bundle.session.to_string(), &session_log_root())?;
    if let Some(store) = bundle.runner.descriptor_store() {
        lease.bind(StoreIdentity::of(store.as_ref()));
    }
    let session = bundle.session;
    let gate: Arc<dyn ModeGate> = bundle.gate;
    // Route the run's live events before the runner is shared, so each
    // response delta, tool progress snapshot, and user notice reaches the
    // serve loop as a session/update instead of only the assembled message at
    // turn end.
    let events = TurnEvents::new();
    events.install_on(&mut bundle.runner);
    let model_options = acp_model_options(&bundle.runner);
    let runner = Arc::new(bundle.runner);
    // The ACP server has no frontend event stream to publish a task's state
    // into, but a command moved to the background still ends after the call
    // that started it: the runner is how its ending reaches the model.
    bundle.background_tasks.attach(None, &runner);
    // The binding a session/new call resolves through. It answers with the
    // runner's own session id and moves the context onto the workspace the
    // client named. Directory grants the fence cannot take are logged, not
    // fatal: the session stays usable and the denial is visible to a
    // developer reading the debug log.
    let binding_runner = Arc::clone(&runner);
    let binding = Arc::new(move |params: &NewSessionRequest| {
        let cwd = PathBuf::from(&params.cwd);
        if cwd.is_absolute() {
            binding_runner.switch_cwd(cwd.clone());
        } else {
            tracing::warn!(cwd = %params.cwd, "session/new cwd is not absolute; keeping the built workspace");
        }
        if let Some(sandbox) = binding_runner.sandbox_session() {
            for dir in &params.additional_directories {
                let path = PathBuf::from(dir);
                if !path.is_absolute() {
                    tracing::warn!(dir = %dir, "additional directory is not absolute; skipped");
                    continue;
                }
                match sandbox.add_working_dir(&path.to_string_lossy()) {
                    Ok(()) => {}
                    Err(e) => {
                        tracing::warn!(dir = %dir, error = %e, "additional directory not granted");
                    }
                }
            }
        }
        session
    });
    let facts_runner = Arc::clone(&runner);
    let facts_gate = Arc::clone(&gate);
    let facts_models = model_options.clone();
    let facts: SessionFacts = Arc::new(move || SessionView {
        modes: session_modes(facts_gate.as_ref()),
        config_options: session_config_options(facts_runner.as_ref(), &facts_models),
    });
    let adapter = Arc::new(
        AcpAdapter::new(AcpxCapabilities::default(), 1, SessionLeaseStore::new())
            .with_session_binding(binding)
            .with_session_facts(facts),
    );
    Ok(AcpSessionParts {
        adapter,
        runner,
        gate,
        session,
        events,
        model_options,
        background_tasks: bundle.background_tasks,
        bus: bundle.bus,
        mcp: bundle
            .mcp
            .take()
            .map(|runtime| Arc::clone(&runtime.manager)),
    })
}

/// Write one reply frame to the client.
async fn send_acp_frame(
    pump: &mut AcpDuplexPump,
    response: &AcpResponse,
) -> Result<(), Box<dyn Error>> {
    let frame = encode(response)?;
    pump.send_frame(frame).await?;
    Ok(())
}

/// The model ids a client may switch to through session/set_config_option:
/// the settings catalog plus the runner's active model, so a resumed
/// session whose model is not in the catalog still has a row.
fn acp_model_options(runner: &Runner) -> Vec<String> {
    let (section, _) =
        houyicoder_config::load_model_section_from(&houyicoder_config::settings_path());
    let mut ids: Vec<String> = section.catalog.into_iter().map(|e| e.id).collect();
    let active = runner.active_model();
    if !ids.iter().any(|m| m == &active) {
        ids.push(active);
    }
    ids
}
