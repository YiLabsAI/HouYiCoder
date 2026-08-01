//! Semantic code graph.
//!
//! LSP-driven semantic graph over symbols / calls / types / dependencies /
//! impact sets. Multi-repo, million-scale. Incremental indexing: a file
//! change produces a delta, so agents query the graph instead of grepping
//! or reading whole files.

#![allow(dead_code)] // crate root re-exports modules consumed by other crates; locally unused

pub mod graph;
pub mod impact;
pub mod index;
pub mod lsp;
pub mod store;
