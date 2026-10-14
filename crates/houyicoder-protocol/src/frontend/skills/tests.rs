//! Tests for this module.

use super::*;

#[test]
fn test_round_trips_camel_case() {
    let entry = SkillEntry {
        name: "commit".into(),
        description: "commit changes".into(),
        origin: "user".into(),
        invocable: true,
        user_invocable: true,
        body_token_estimate: 120,
        usage: Some(SkillUsage {
            invocations: 3,
            refusals: 1,
            last_used_secs: 1_700_000_000,
        }),
    };
    let json = serde_json::to_string(&entry).expect("serialize");
    assert!(json.contains("\"name\":\"commit\""), "{json}");
    assert!(json.contains("\"origin\":\"user\""), "{json}");
    assert!(json.contains("\"invocable\":true"), "{json}");
    assert!(json.contains("\"bodyTokenEstimate\":120"), "{json}");
    assert!(json.contains("\"invocations\":3"), "{json}");
    let back: SkillEntry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, entry);
}

/// Old payloads without the usage field deserialize to None (backward
/// compat: a stale client or old log does not break).
#[test]
fn test_old_payload_defaults_none() {
    let json =
        r#"{"name":"x","description":"d","origin":"user","invocable":true,"bodyTokenEstimate":0}"#;
    let entry: SkillEntry = serde_json::from_str(json).expect("deserialize");
    assert!(entry.usage.is_none(), "old payload: usage defaults to None");
}
