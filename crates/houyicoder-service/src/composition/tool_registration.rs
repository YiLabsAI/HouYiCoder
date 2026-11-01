//! How provider tools reach the registry. The built-in provider builds its
//! tools already carrying the permission gate where they need it; an external
//! provider hands over bare adapters, which this module wraps before
//! registering them. A tool whose name the registry already holds is refused,
//! so a server cannot take the name the model already addresses.

use std::sync::Arc;

use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_api::mcp::{McpClient, McpError, McpToolProvider};
use houyicoder_api::tool::ToolProvider;
use houyicoder_config::{McpServerConfig, resolve_mcp_servers};
use houyicoder_core::agent::ToolRegistry;
use houyicoder_permission::{GuardedTool, ModeGate};

use super::built_in_tools::BuiltInToolProvider;

/// Register the built-in provider and any configured external tool servers.
///
/// An external provider hands over bare adapters, so those are wrapped here and
/// then judged by the same gate a built-in call faces. The gate's own rules
/// decide the verdict, including which tool names receive the path boundary
/// check.
pub(super) fn register_provider_tools(
    tools: &mut ToolRegistry,
    gate: Arc<dyn ModeGate>,
    builtin: BuiltInToolProvider,
) {
    let servers = resolve_mcp_servers();
    let external = start_servers(&servers);
    register_in_order(tools, &gate, &builtin, &external);
}

/// Start every configured external server, reporting one that fails and
/// carrying on with the rest: one broken configuration does not cost the
/// session its other tools.
fn start_servers(servers: &[McpServerConfig]) -> Vec<Box<dyn ToolProvider>> {
    let mut started = Vec::new();
    for cfg in servers {
        match start_server(cfg) {
            Ok(provider) => started.push(Box::new(provider) as Box<dyn ToolProvider>),
            Err(e) => {
                let program = &cfg.program;
                tracing::warn!(
                    "external tool server {program:?} failed to start: {e}; \
                     skipping (built-in tools still load)"
                );
            }
        }
    }
    started
}

/// Start one configured external server and its tool provider.
fn start_server(cfg: &McpServerConfig) -> Result<McpToolProvider, McpError> {
    let launcher = StdProcessLauncher::new();
    let (client, entries) = McpClient::spawn(&cfg.program, &cfg.args, &launcher)?;
    Ok(McpToolProvider::new(client, entries))
}

/// Register the built-in provider first, then each external provider behind the
/// permission gate.
///
/// The order is the admission policy: a name the built-in set holds is taken
/// by the time an external provider arrives, so a server cannot shadow a
/// built-in tool with a different implementation under the name the model
/// already addresses.
fn register_in_order(
    tools: &mut ToolRegistry,
    gate: &Arc<dyn ModeGate>,
    builtin: &dyn ToolProvider,
    external: &[Box<dyn ToolProvider>],
) {
    for tool in builtin.tools() {
        tools.register(tool);
    }
    for provider in external {
        register_guarded_provider(tools, provider.as_ref(), gate);
    }
}

/// Register one external provider's tools behind the permission gate. A tool
/// whose name is already taken is refused rather than replacing the registered
/// one.
fn register_guarded_provider(
    tools: &mut ToolRegistry,
    provider: &dyn ToolProvider,
    gate: &Arc<dyn ModeGate>,
) {
    for tool in provider.tools() {
        let name = tool.name();
        if tools.contains(name) {
            let provider_name = provider.name();
            tracing::warn!(
                "external tool {name:?} from provider {provider_name:?} collides with a \
                 registered tool; keeping the registered tool"
            );
            continue;
        }
        tools.register(Arc::new(GuardedTool::new(tool, Arc::clone(gate))));
    }
}

#[cfg(test)]
mod tests;
