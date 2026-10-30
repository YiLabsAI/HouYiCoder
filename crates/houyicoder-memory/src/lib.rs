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
pub use local_file::{LocalFileBackend, ReclaimError, ReclaimPolicy, ReclaimReport, ReclaimStop};
pub use markdown_memory::MarkdownMemoryProvider;
pub use native::KeywordRecallProvider;
pub use reranker::SemanticReranker;

use houyicoder_context::BlockHash;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// Bytes hashed per read when verifying a stored block. A block can be large,
/// so the check reads it through one fixed buffer instead of loading a second
/// copy into memory.
const HASH_BUFFER_BYTES: usize = 64 * 1024;

/// Compute the SHA-256 content hash of a blob and return it as a lowercase
/// hex BlockHash. Same bytes always yield the same hash; this is the CAS
/// content-addressing key shared by both backends.
pub(crate) fn sha256_hex(bytes: &[u8]) -> BlockHash {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(&hasher.finalize())
}

/// Compute the SHA-256 of a file's bytes without holding the file in memory,
/// so verifying a block already on disk costs one buffer. The digest is the
/// same one hashing the whole content returns.
pub(crate) fn sha256_file(path: &Path) -> io::Result<BlockHash> {
    sha256_reader(File::open(path)?)
}

/// Compute the SHA-256 of a reader's bytes. A caller that already holds the
/// file open hashes that handle rather than reopening the path, so the bytes
/// it verifies are the bytes it then reads.
pub(crate) fn sha256_reader(mut reader: impl Read) -> io::Result<BlockHash> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; HASH_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(&hasher.finalize()))
}

fn hex_digest(digest: &[u8]) -> BlockHash {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        hex.push(DIGITS[usize::from(b >> 4)] as char);
        hex.push(DIGITS[usize::from(b & 0x0f)] as char);
    }
    BlockHash(hex)
}

#[cfg(test)]
mod tests;
