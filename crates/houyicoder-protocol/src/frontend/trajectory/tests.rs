//! Tests for this module.

use super::*;

fn fixture() -> TrajectoryEntry {
    TrajectoryEntry {
        kind: "assistant".into(),
        ts: 1_700_000_000,
        event_id: "01HXYZ".into(),
        prev_hash: Some("0123456789abcdef".into()),
        duration_ms: None,
        usage: None,
    }
}

#[test]
fn test_round_trips() {
    let original = fixture();
    let json = serde_json::to_string(&original).expect("serialize");
    let back: TrajectoryEntry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, original);
}

#[test]
fn test_uses_camel_case_keys() {
    let json = serde_json::to_string(&fixture()).expect("serialize");
    assert!(json.contains("\"eventId\""), "camelCase: {json}");
    assert!(json.contains("\"prevHash\""), "camelCase: {json}");
    assert!(!json.contains("event_id"), "snake leaked: {json}");
}

#[test]
fn test_usage_round_trips() {
    let mut entry = fixture();
    entry.usage = Some(TrajectoryUsage {
        model: "small".into(),
        input_tokens: Some(900),
        output_tokens: Some(40),
        cache_read_input_tokens: Some(800),
        cache_write_input_tokens: None,
    });
    let json = serde_json::to_string(&entry).expect("serialize");
    assert!(json.contains("\"inputTokens\":900"), "camelCase: {json}");
    assert!(
        !json.contains("cacheWriteInputTokens"),
        "an unknown figure is omitted, not zero: {json}"
    );
    let back: TrajectoryEntry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, entry);
}

#[test]
fn test_genesis_entry_omits_hash() {
    let mut e = fixture();
    e.prev_hash = None;
    let json = serde_json::to_string(&e).expect("serialize");
    assert!(!json.contains("prevHash"), "None skipped: {json}");
    let back: TrajectoryEntry = serde_json::from_str(&json).expect("deserialize");
    assert!(back.prev_hash.is_none());
}
