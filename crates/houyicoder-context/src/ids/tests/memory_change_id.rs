//! Tests for this module.

use super::super::*;

#[test]
fn test_memory_id_round_trips() {
    let id = MemoryChangeId::default();
    assert_ne!(id, MemoryChangeId::new());
    let rendered = id.to_string();
    assert_eq!(MemoryChangeId::from_display_string(&rendered), Some(id));
    let json = serde_json::to_string(&id).expect("serialize memory change id");
    assert_eq!(
        serde_json::from_str::<MemoryChangeId>(&json).expect("deserialize memory change id"),
        id
    );
}

#[test]
fn test_memory_id_rejects_invalid() {
    assert!(MemoryChangeId::from_display_string("not-an-id").is_none());
}
