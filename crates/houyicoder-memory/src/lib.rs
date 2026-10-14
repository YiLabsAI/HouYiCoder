//! Memory provider.
//!
//! Defines the MemoryProvider trait (the abstraction the engine depends on)
//! and provides several implementations:
//! - markdown_memory: a markdown-directory store with a derived index,
//!   deterministic recall, and single-source atomic write (de-modelized
//!   recall, atomic single-source write, no render-path sync read).
//! - native: a from-scratch in-process store for the minimal/no-sidecar path.
//! - houyi: a Python sidecar memory engine over JSON-RPC/stdio or HTTP.
//!   Not reimplemented.
//!
//! The sidecar is plugged in exactly like an MCP server — a guest via the
//! extension protocol — so the layering stays clean.

#![allow(dead_code)] // crate root re-exports backends consumed by other crates; locally unused

pub mod descriptor_store;
pub mod houyi;
pub mod in_memory;
pub mod local_file;
pub mod markdown_memory;
pub mod native;
pub mod provider;
pub mod reranker;

pub use descriptor_store::{FileDescriptorStore, InMemoryDescriptorStore};
pub use houyi::StubMemoryProvider;
pub use in_memory::InMemoryBackend;
pub use local_file::LocalFileBackend;
pub use markdown_memory::MarkdownMemoryProvider;
pub use native::KeywordRecallProvider;
pub use reranker::SemanticReranker;

use houyicoder_context::BlockHash;
use sha2::{Digest, Sha256};

/// Compute the SHA-256 content hash of a blob and return it as a lowercase
/// hex BlockHash. Same bytes always yield the same hash; this is the CAS
/// content-addressing key shared by both backends.
pub(crate) fn sha256_hex(bytes: &[u8]) -> BlockHash {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    BlockHash(hex)
}

#[cfg(test)]
mod tests;
