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
