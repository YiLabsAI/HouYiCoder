//! The MCP client boundary: external tool servers discovered, connected, and
//! adapted into the tools this host runs. The SDK this module wraps and the
//! transport it speaks stay inside it; every other layer holds the workspace's
//! own types, a tool, a status view, or an instruction block.

mod client;
mod http;
mod liveness;
mod manager;
mod resources;
mod tools;

pub use liveness::RestartPolicy;
pub use manager::{McpConnections, McpTimings};
// The no-spawn launcher a unit test builds a manager with, shared by the
// manager, tool, and server-handler test modules.
#[cfg(test)]
pub(crate) use manager::tests::NoSpawn;
pub(crate) use resources::{
    LIST_RESOURCES_TOOL, ListMcpResourcesTool, READ_RESOURCE_TOOL, ReadMcpResourceTool,
};
pub use tools::McpToolSource;

use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_api::session::SessionLog;
use houyicoder_config::resolve_scoped_mcp_servers;
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
/// registry reads. The caller supplies the runtime the supervisors run on
/// and the env spelling of the server list, so assembly often happens on a
/// plain thread and no ambient handle or process variable is assumed. None
/// when no runtime is supplied; configured servers are then named in a
/// diagnostic rather than dropped in silence.
///
/// This is the one place the settings, the environment, and the workspace's
/// trust answer meet: the resolver merges the scopes, disables an untrusted
/// workspace's project servers before anything is expanded, and the manager
/// is told what to start.
pub fn start_servers(
    settings_path: &Path,
    workspace: Option<&Path>,
    trusted: bool,
    env_raw: Option<&str>,
    handle: Option<&Handle>,
    gate: Arc<dyn ModeGate>,
    log: Arc<dyn SessionLog>,
) -> Option<McpRuntime> {
    // A value a server entry references through ${VAR} is read here rather
    // than at the entry: the expansion contract names the process env.
    let lookup = |name: &str| env::var(name).ok();
    let servers = resolve_scoped_mcp_servers(settings_path, workspace, env_raw, trusted, &lookup);
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
    start_resolved(handle, servers, trusted, workspace, gate, log)
}

/// Start a runtime over an already resolved server list, or None when the
/// list is empty. Splitting the resolution from the start keeps the launch
/// decision a function of its inputs, so the environment and the settings
/// file are read once at the composition root.
fn start_resolved(
    handle: &Handle,
    servers: Vec<ScopedMcpServerConfig>,
    trusted: bool,
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
    // Spawning through the handle needs the handle entered on this thread;
    // the guard restores the previous context when it drops.
    let _entered = handle.enter();
    let source = manager.tool_source(gate, Some(log));
    let start = Arc::clone(&manager);
    handle.spawn(async move {
        start.start_all(servers, trusted).await;
    });
    Some(McpRuntime { manager, source })
}

#[cfg(test)]
mod tests;
