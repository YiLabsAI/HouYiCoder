//! The language server boundary: servers discovered in the configuration,
//! launched lazily, spoken to over framed JSON-RPC, and adapted into the
//! tools and diagnostics this host runs. Every other layer holds the
//! workspace's own types, a status view, or a formatted answer.

pub(crate) mod client;
mod framing;
mod manager;

pub use manager::{LspConnections, LspTimings};
