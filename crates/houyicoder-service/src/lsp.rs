//! The language server boundary: servers discovered in the configuration,
//! launched lazily, spoken to over framed JSON-RPC, and adapted into the
//! tools and diagnostics this host runs. Every other layer holds the
//! workspace's own types, a status view, or a formatted answer.

pub(crate) mod client;
pub(crate) mod diagnostics;
mod framing;
mod manager;
mod tool;

pub(crate) use diagnostics::SaveDiagnosticsTool;
#[cfg(test)]
pub(crate) use manager::tests::NoSpawn;
pub use manager::{LspConnections, LspTimings};
pub(crate) use tool::LspTool;

#[cfg(test)]
pub(crate) use houyicoder_protocol::lsp::{LspConfigScope, ScopedLspServerConfig};

/// A server config whose shutdown timeout fits a unit test: the mock peer
/// never sends an exit notice, so the production 2-second wait would dominate
/// every test. One helper means the timeout lives in one place, not in each
/// test module's fixture.
#[cfg(test)]
pub(crate) fn fast_server_config(name: &str) -> ScopedLspServerConfig {
    let mut server = ScopedLspServerConfig::stdio("fake", Default::default());
    server.name = name.into();
    server.scope = LspConfigScope::User;
    server.shutdown_timeout_ms = 5;
    server
        .extension_to_language
        .insert(".rs".into(), "rust".into());
    server
}
