//! Tests for this module.

use super::*;

#[test]
fn test_round_trips_camel_case() {
    let entry = ToolEntry {
        name: "bash".into(),
        description: "run a shell command".into(),
    };
    let json = serde_json::to_string(&entry).expect("serialize");
    assert!(json.contains("\"name\":\"bash\""), "{json}");
    assert!(json.contains("\"description\":\"run"), "{json}");
    let back: ToolEntry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, entry);
}
