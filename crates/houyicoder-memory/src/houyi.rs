//! Sidecar-backed provider stub. The real backend is not wired yet; recall
//! returns an empty Vec and add succeeds as a no-op until that
//! contract is implemented.

use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::{MemoryEntry, MemoryError};
use std::collections::HashSet;

/// Empty recall provider: a placeholder for a backend not yet wired.
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
    fn recall(
        &self,
        _query: &str,
        _budget: usize,
        _surfaced: &HashSet<String>,
    ) -> Vec<MemoryEntry> {
        Vec::new()
    }

    fn add(&self, _entry: MemoryEntry) -> Result<(), MemoryError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::MemorySource;

    #[test]
    fn test_stub_recall_returns_empty() {
        let provider = StubMemoryProvider::new();
        assert!(
            provider
                .recall("anything", 1000, &HashSet::new())
                .is_empty()
        );
    }

    #[test]
    fn test_stub_recall_empty_store() {
        let provider = StubMemoryProvider::new();
        assert!(provider.recall("", 0, &HashSet::new()).is_empty());
    }

    #[test]
    fn test_stub_add_succeeds() {
        let provider = StubMemoryProvider::new();
        let e = MemoryEntry::new("k", "content", MemorySource::User);
        assert!(provider.add(e).is_ok());
    }
}
