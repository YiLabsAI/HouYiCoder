//! Wiring the external tool servers into one session: resolve the settings
//! and the environment, start the runtime, and attach what it exposes to the
//! tool registry. The composition root calls this once per session, so the
//! trust answer and the servers it gates always come from the same file.

use std::path::Path;
use std::sync::Arc;

use houyicoder_api::session::SessionLog;
use houyicoder_config::settings_path;
use houyicoder_config::settings_path_in;
use houyicoder_config::trust::is_path_trusted;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::ToolSource;
use houyicoder_permission::GuardedTool;
use houyicoder_permission::ModeGate;
use tokio::runtime::Handle;

use crate::mcp;
use crate::mcp::ListMcpResourcesTool;
use crate::mcp::McpRuntime;
use crate::mcp::ReadMcpResourceTool;

/// Start the configured servers for one session and attach them to the
/// registry: the live tool source plus the two host tools that read a
/// server's resources. An untrusted workspace's project entries are disabled
/// by the resolver before anything is launched.
pub(super) fn start_and_attach(
    tools: ToolRegistry,
    home: Option<&Path>,
    workspace: Option<&Path>,
    env_servers: Option<&str>,
    runtime: Option<&Handle>,
    gate: &Arc<dyn ModeGate>,
    log: Arc<dyn SessionLog>,
) -> (ToolRegistry, Option<McpRuntime>) {
    // A caller that routed its state through a home must see that home's
    // settings, or an untrusted workspace's servers would be judged against
    // somebody else's trust list.
    let settings_file = match home {
        Some(home) => settings_path_in(home),
        None => settings_path(),
    };
    let trusted = workspace.is_none_or(|workspace| is_path_trusted(&settings_file, workspace));
    let started = mcp::start_servers(
        &settings_file,
        workspace,
        trusted,
        env_servers,
        runtime,
        Arc::clone(gate),
        Arc::clone(&log),
    );
    let Some(runtime) = started else {
        return (tools, None);
    };
    let mut tools = tools.with_source(Arc::clone(&runtime.source) as Arc<dyn ToolSource>);
    let list = ListMcpResourcesTool::new(Arc::clone(&runtime.manager), Some(Arc::clone(&log)));
    let read = ReadMcpResourceTool::new(Arc::clone(&runtime.manager), Some(log));
    tools.register(Arc::new(GuardedTool::new(Arc::new(list), Arc::clone(gate))));
    tools.register(Arc::new(GuardedTool::new(Arc::new(read), Arc::clone(gate))));
    (tools, Some(runtime))
}

#[cfg(test)]
mod tests;
