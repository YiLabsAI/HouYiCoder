//! Tests for this module.

use super::*;
use houyicoder_context::MemorySource;

/// Every port must support runtime dispatch.
#[test]
fn test_traits_are_object_safe() {
    let _provider: Box<dyn MemoryProvider> = Box::new(Stub);
    let _store: Box<dyn MemoryStore> = Box::new(Stub);
    let _reranker: Box<dyn MemoryReranker> = Box::new(StubReranker);
}

struct StubReranker;
impl MemoryReranker for StubReranker {
    fn rerank(
        &self,
        _query: &str,
        _candidates: &[MemoryRankHit],
        _limit: usize,
    ) -> PFut<'_, RerankOutcome> {
        Box::pin(async { RerankOutcome::Selected(Vec::new()) })
    }
}

/// The default delete_memory_in_scope delegates to delete_memory so a
/// single-root provider (the Stub) treats every scope as the one root.
#[test]
fn test_default_delete_in_scope() {
    let stub = Stub;
    assert!(
        matches!(
            stub.delete_memory_in_scope("any", MemoryScope::Project),
            Err(MemoryError::NotFound)
        ),
        "default delegates to delete_memory (NotFound for the Stub)"
    );
}

struct Stub;
impl MemoryProvider for Stub {
    fn add(&self, _entry: MemoryEntry) -> Result<(), MemoryError> {
        Ok(())
    }
}
impl MemoryStore for Stub {
    fn scan(&self) -> Vec<MemoryEntry> {
        Vec::new()
    }
    fn read(&self, _key: &str) -> Result<MemoryEntry, MemoryError> {
        Err(MemoryError::NotFound)
    }
    fn write(&self, _entry: MemoryEntry) -> Result<(), MemoryError> {
        Ok(())
    }
    fn rebuild(&self) -> Result<(), MemoryError> {
        Ok(())
    }
}

#[test]
fn test_store_stub_round_trip() {
    let store = Stub;
    assert!(store.scan().is_empty(), "scan returns empty by default");
    assert!(
        matches!(store.read("any"), Err(MemoryError::NotFound)),
        "read returns NotFound for the stub"
    );
    let entry = MemoryEntry::new("k", "content", MemorySource::User);
    assert!(store.write(entry).is_ok(), "write accepts the entry");
    assert!(store.rebuild().is_ok(), "rebuild succeeds");
}
