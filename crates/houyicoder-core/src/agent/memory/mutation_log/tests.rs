//! Tests for this module.

use super::*;

#[test]
fn test_log_drains_successful_changes() {
    let log = MutationLog::new();
    log.record("key", MemoryOperation::Created, MemoryScope::Auto);
    assert_eq!(log.take().len(), 1);
    assert!(log.take().is_empty());
}

#[test]
fn test_merge_collapses_key() {
    let log = MutationLog::new();
    log.record("a", MemoryOperation::Created, MemoryScope::Auto);
    log.record("a", MemoryOperation::Updated, MemoryScope::Auto);
    log.record("b", MemoryOperation::Created, MemoryScope::Auto);
    log.record("b", MemoryOperation::Promoted, MemoryScope::Project);
    log.record("c", MemoryOperation::Created, MemoryScope::Auto);
    log.record("c", MemoryOperation::Deleted, MemoryScope::Auto);
    log.record("d", MemoryOperation::Updated, MemoryScope::Project);
    let merged = log.take_merged();
    assert_eq!(
        merged,
        vec![
            MemoryChange {
                key: "a".into(),
                operation: MemoryOperation::Created,
                scope: MemoryScope::Auto,
            },
            MemoryChange {
                key: "b".into(),
                operation: MemoryOperation::Promoted,
                scope: MemoryScope::Project,
            },
            MemoryChange {
                key: "d".into(),
                operation: MemoryOperation::Updated,
                scope: MemoryScope::Project,
            },
        ]
    );
}
