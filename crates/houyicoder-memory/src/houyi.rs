//! Sidecar-backed provider stub. The real backend is not connected yet; the
//! rank default returns no candidates and add succeeds as a no-op until
//! that contract is implemented.

use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::{MemoryEntry, MemoryError};

/// A placeholder rank provider for a backend not yet connected.
pub struct StubMemoryProvider;

impl StubMemoryProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StubMemoryProvider {
    fn default() -> Self {
        Self
    }
}

impl MemoryProvider for StubMemoryProvider {
    fn add(&self, _entry: MemoryEntry) -> Result<(), MemoryError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
