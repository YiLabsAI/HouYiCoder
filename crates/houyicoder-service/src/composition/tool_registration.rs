//! How provider tools reach the registry. The built-in provider builds its
//! tools already carrying the permission gate where they need it. External
//! servers reach the registry through a live source instead: their adapters
//! arrive as the servers connect, so registration is a static pass for the
//! built-in set and the source is consulted per turn.

use houyicoder_api::tool::ToolProvider;
use houyicoder_core::agent::ToolRegistry;

use super::built_in_tools::BuiltInToolProvider;

/// Register the built-in provider's tools.
///
/// The order is the admission policy: a name the built-in set holds is taken
/// by the time any server tool arrives through the live source, and the
/// registry refuses a source tool whose name is already registered, so a
/// server cannot shadow a built-in tool with a different implementation under
/// the name the model already addresses.
pub(super) fn register_provider_tools(tools: &mut ToolRegistry, builtin: BuiltInToolProvider) {
    for tool in builtin.tools() {
        tools.register(tool);
    }
}

#[cfg(test)]
mod tests;
