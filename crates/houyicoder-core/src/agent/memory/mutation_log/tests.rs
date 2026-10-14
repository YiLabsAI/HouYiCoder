//! Tests for this module.

use super::*;

#[test]
fn test_log_drains_successful_changes() {
    let log = MutationLog::new();
    log.record("key", MemoryOperation::Created, MemoryScope::Auto);
    assert_eq!(log.take().len(), 1);
    assert!(log.take().is_empty());
}
