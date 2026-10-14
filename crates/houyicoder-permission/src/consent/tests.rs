use super::*;

#[test]
fn test_recall_miss_when_empty() {
    let s = InMemoryConsentStore::non_expiring();
    assert!(!s.recall("edit", ""));
}

#[test]
fn test_record_then_recall_hits() {
    let s = InMemoryConsentStore::non_expiring();
    s.record("edit", "k1");
    assert!(s.recall("edit", "k1"));
}

#[test]
fn test_recall_misses_on_different() {
    let s = InMemoryConsentStore::non_expiring();
    s.record("edit", "k1");
    assert!(!s.recall("edit", "k2"));
    assert!(!s.recall("write", "k1"));
}

#[test]
fn test_record_refreshes_ttl() {
    let s = InMemoryConsentStore::new(Duration::from_millis(50));
    s.record("edit", "k1");
    s.advance_clock(Duration::from_millis(30));
    s.record("edit", "k1"); // refresh
    s.advance_clock(Duration::from_millis(30));
    assert!(s.recall("edit", "k1")); // still fresh after refresh
}

#[test]
fn test_entry_expires_after_ttl() {
    let s = InMemoryConsentStore::new(Duration::from_millis(20));
    s.record("edit", "k1");
    assert!(s.recall("edit", "k1"));
    s.advance_clock(Duration::from_millis(30));
    assert!(!s.recall("edit", "k1")); // expired
}

#[test]
fn test_advance_clock_expires_entry() {
    let s = InMemoryConsentStore::new(Duration::from_secs(10));
    s.record("edit", "k1");
    assert!(s.recall("edit", "k1"));
    s.advance_clock(Duration::from_secs(20));
    assert!(!s.recall("edit", "k1"));
}

#[test]
fn test_expired_entries_pruned() {
    let s = InMemoryConsentStore::new(Duration::from_millis(10));
    s.record("edit", "expired");
    s.advance_clock(Duration::from_millis(20));
    s.recall("edit", "expired"); // triggers prune
    assert_eq!(s.entries.lock().unwrap().len(), 0);
}

#[test]
fn test_args_key_stable_same() {
    let v = serde_json::json!({"path": "/tmp/a"});
    assert_eq!(args_key(Some(&v)), args_key(Some(&v)));
}

#[test]
fn test_args_key_differs_input() {
    let a = serde_json::json!({"path": "/tmp/a"});
    let b = serde_json::json!({"path": "/tmp/b"});
    assert_ne!(args_key(Some(&a)), args_key(Some(&b)));
}

#[test]
fn test_args_key_none_empty() {
    assert_eq!(args_key(None), "");
}
