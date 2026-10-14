//! Tests for this module.

use super::*;
use houyicoder_context::MemorySource;
use std::collections::HashSet;

#[test]
fn test_stub_rank_returns_empty() {
    let provider = StubMemoryProvider::new();
    assert!(
        provider
            .rank_candidates("anything", &HashSet::new())
            .is_empty()
    );
}

#[test]
fn test_stub_add_succeeds() {
    let provider = StubMemoryProvider::new();
    let e = MemoryEntry::new("k", "content", MemorySource::User);
    assert!(provider.add(e).is_ok());
}
