//! Tests for this module.

use super::*;

#[test]
fn test_summary_round_trips() {
    let entry = MemorySummaryEntry {
        key: "build-gate".into(),
        description: "make check must stay green".into(),
        source: "project".into(),
        scope: "project".into(),
        mtime_secs: 0,
    };
    let json = serde_json::to_string(&entry).expect("serialize");
    assert!(json.contains("\"key\":\"build-gate\""), "{json}");
    assert!(json.contains("\"mtimeSecs\""), "camelCase: {json}");
    assert!(json.contains("\"scope\":\"project\""), "{json}");
    let back: MemorySummaryEntry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, entry);
}

#[test]
fn test_entry_round_trips() {
    let entry = MemoryDetail {
        key: "k".into(),
        content: "body".into(),
        source: "user".into(),
        description: "d".into(),
        mtime_secs: 99,
    };
    let json = serde_json::to_string(&entry).expect("serialize");
    assert!(json.contains("\"mtimeSecs\":99"), "{json}");
    let back: MemoryDetail = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, entry);
}

#[test]
fn test_scope_serializes_lowercase() {
    for (value, expect, label) in [
        (MemoryChangeScope::User, "\"user\"", Some("user")),
        (MemoryChangeScope::Project, "\"project\"", Some("project")),
        (MemoryChangeScope::Auto, "\"auto\"", Some("auto")),
        (MemoryChangeScope::Unknown, "\"unknown\"", None),
    ] {
        let json = serde_json::to_string(&value).expect("serialize");
        assert_eq!(json, expect);
        // The rendered label is the wire tag, so a rename of the tag
        // cannot leave the notice naming a root the frame never carried.
        assert_eq!(value.label(), label, "label follows the wire tag");
        let back: MemoryChangeScope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }
}

#[test]
fn test_unrecognized_scope_reads_unknown() {
    // A scope tag this build does not name lands on Unknown rather than
    // failing the decode. The catch-all is what keeps a future scope
    // from dropping the whole change, and it is separate from the
    // absent-field default that test_missing_scope_keeps_change covers.
    let scope: MemoryChangeScope =
        serde_json::from_str("\"global\"").expect("decode unrecognized tag");
    assert_eq!(scope, MemoryChangeScope::Unknown);
    assert_eq!(scope.label(), None, "an unknown scope names no root");
}

#[test]
fn test_legacy_stored_reads_unknown() {
    // The Stored tag predates the Created/Updated split. A receiver must
    // still decode a legacy stored tag as Unknown so the key reaches the
    // notice rather than dropping the change.
    let op: MemoryOperation = serde_json::from_str("\"stored\"").expect("decode legacy tag");
    assert_eq!(op, MemoryOperation::Unknown);
}

#[test]
fn test_change_round_trips_scope() {
    let change = MemoryChange {
        key: "build-gate".into(),
        operation: MemoryOperation::Promoted,
        scope: MemoryChangeScope::Project,
    };
    let json = serde_json::to_string(&change).expect("serialize");
    assert_eq!(
        json,
        "{\"key\":\"build-gate\",\"operation\":\"promoted\",\"scope\":\"project\"}"
    );
    let back: MemoryChange = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, change);
}

#[test]
fn test_causality_serializes_kebab_case() {
    for (value, expect) in [
        (MemoryChangeCausality::ThisTurn, "\"this-turn\""),
        (MemoryChangeCausality::PreviousTurn, "\"previous-turn\""),
        (MemoryChangeCausality::Unknown, "\"unknown\""),
    ] {
        let json = serde_json::to_string(&value).expect("serialize");
        assert_eq!(json, expect);
        let back: MemoryChangeCausality = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }
}

#[test]
fn test_toggle_which_serializes_lowercase() {
    for (which, expect) in [
        (MemoryToggleWhich::Auto, "\"auto\""),
        (MemoryToggleWhich::Dream, "\"dream\""),
    ] {
        let json = serde_json::to_string(&which).expect("serialize");
        assert_eq!(json, expect);
        let back: MemoryToggleWhich = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, which);
    }
}

#[test]
fn test_toggle_state_round_trips() {
    let state = ToggleState {
        auto_memory: true,
        auto_dream: false,
    };
    let json = serde_json::to_string(&state).expect("serialize");
    assert!(json.contains("\"autoMemory\":true"), "{json}");
    assert!(json.contains("\"autoDream\":false"), "{json}");
    let back: ToggleState = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, state);
}
