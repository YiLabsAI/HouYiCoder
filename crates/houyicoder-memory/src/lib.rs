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

pub mod houyi;
pub mod in_memory;
pub mod local_file;
pub mod markdown_memory;
pub mod meta_store;
pub mod native;
pub mod provider;

pub use houyi::StubMemoryProvider;
pub use in_memory::InMemoryBackend;
pub use local_file::LocalFileBackend;
pub use markdown_memory::MarkdownMemoryProvider;
pub use meta_store::{FileMetaStore, InMemoryMetaStore};
pub use native::KeywordRecallProvider;

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
mod tests {
    use super::*;

    // Known SHA-256 vector: sha256("abc") = ba7816bf... (FIPS 180-2 test 2).
    #[test]
    fn test_sha256_hex_known_vector() {
        let BlockHash(hex) = sha256_hex(b"abc");
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn test_sha256_hex_empty_input() {
        let BlockHash(hex) = sha256_hex(b"");
        assert_eq!(
            hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(hex.len(), 64);
    }
}
