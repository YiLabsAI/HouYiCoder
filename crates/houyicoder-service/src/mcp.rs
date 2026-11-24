//! The MCP client boundary: external tool servers discovered, connected, and
//! adapted into the tools this host runs. The SDK this module wraps and the
//! transport it speaks stay inside it; every other layer holds the workspace's
//! own types, a tool, a status view, or an instruction block.

mod client;
mod liveness;
mod manager;
mod stdio_bridge;
mod tools;

pub use client::{ClientEvent, HouyiMcpClientHandler, LaunchedMcpServer, launch_stdio};
pub use liveness::RestartPolicy;
pub use manager::{McpConnections, McpTimings};
pub use tools::{McpToolSource, registered_name, start_mirror};

use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_api::session::SessionLog;
use houyicoder_config::{ENV_HOUYICODER_MCP_SERVERS, resolve_scoped_mcp_servers};
use houyicoder_permission::ModeGate;
use houyicoder_platform::tls::install_tls_provider;
use houyicoder_protocol::mcp::ScopedMcpServerConfig;
use tokio::runtime::Handle;

/// The live tools one assembled session carries, the manager that
/// supervises the servers and the source the registry consults.
pub struct McpRuntime {
    pub manager: Arc<McpConnections>,
    pub source: Arc<McpToolSource>,
}

/// Start the configured servers for one session and return the tools the
/// registry reads. The caller supplies the runtime the supervisors run on:
/// assembly often happens on a plain thread before the process runtime is
/// first used, so an ambient handle cannot be assumed. None when no runtime is
/// supplied; configured servers are then named in a diagnostic rather than
/// dropped in silence.
///
/// This is the one place the settings, the environment, and the workspace's
/// trust answer meet: the resolver merges the scopes, disables an untrusted
/// workspace's project servers before anything is expanded, and the manager
/// is told what to start.
pub fn start_servers(
    settings_path: &Path,
    workspace: Option<&Path>,
    trusted: bool,
    handle: Option<&Handle>,
    gate: Arc<dyn ModeGate>,
    log: Arc<dyn SessionLog>,
) -> Option<McpRuntime> {
    let env_raw = env::var(ENV_HOUYICODER_MCP_SERVERS).ok();
    let lookup = |name: &str| env::var(name).ok();
    let servers = resolve_scoped_mcp_servers(
        settings_path,
        workspace,
        env_raw.as_deref(),
        trusted,
        &lookup,
    );
    let Some(handle) = handle else {
        if !servers.is_empty() {
            tracing::warn!(
                count = servers.len(),
                "no async runtime was supplied at assembly, so the configured \
                 external tool servers were not started"
            );
        }
        return None;
    };
    start_resolved(handle, servers, workspace, gate, log)
}

/// Start a runtime over an already resolved server list, or None when the
/// list is empty. Splitting the resolution from the start keeps the launch
/// decision a function of its inputs, so the environment and the settings
/// file are read once at the composition root.
fn start_resolved(
    handle: &Handle,
    servers: Vec<ScopedMcpServerConfig>,
    workspace: Option<&Path>,
    gate: Arc<dyn ModeGate>,
    log: Arc<dyn SessionLog>,
) -> Option<McpRuntime> {
    if servers.is_empty() {
        return None;
    }
    // The HTTP pool ships without a crypto backend; a server that reaches an
    // endpoint needs the process-wide provider installed first.
    install_tls_provider();
    // A user-scope server starts in the process directory when no workspace is
    // given; the filesystem root would put it where none of the user's files are.
    let root: Arc<Path> = Arc::from(
        workspace
            .map(Path::to_path_buf)
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from(".")),
    );
    let manager = Arc::new(McpConnections::new(
        Arc::new(StdProcessLauncher::new()),
        root,
        McpTimings::default(),
    ));
    let source = Arc::new(McpToolSource::default());
    // Spawning through the handle needs the handle entered on this thread;
    // the guard restores the previous context when it drops.
    let _entered = handle.enter();
    let start = manager.clone();
    handle.spawn(async move {
        start.start_all(servers).await;
    });
    drop(start_mirror(
        (*manager).clone(),
        Arc::clone(&source),
        gate,
        Some(log),
    ));
    Some(McpRuntime { manager, source })
}

#[cfg(test)]
mod tests;

pub use stdio_bridge::StderrTail;
