//! Tests for this module.

use super::*;
use crate::frontend::memory::{MemoryChangeScope, MemoryOperation};

#[test]
fn test_memory_event_round_trips() {
    let event = FrontendEvent::MemoryChanged {
        id: MemoryChangeId("change-1".into()),
        origin: MemoryChangeOrigin::AutoMemory,
        causality: MemoryChangeCausality::PreviousTurn,
        changes: vec![MemoryChange {
            key: "build-gate".into(),
            operation: MemoryOperation::Created,
            scope: MemoryChangeScope::Auto,
        }],
    };
    let json = serde_json::to_string(&event).expect("serialize memory event");
    assert!(
        json.contains("\"operation\":\"created\""),
        "Created serializes as kebab-case created: {json}"
    );
    assert!(
        json.contains("\"causality\":\"previous-turn\""),
        "causality serializes kebab-case: {json}"
    );
    assert!(
        json.contains("\"scope\":\"auto\""),
        "scope serializes lowercase: {json}"
    );
    assert!(
        !json.contains("\"stored\""),
        "no Stored variant leaks to the wire: {json}"
    );
    let decoded = serde_json::from_str::<FrontendEvent>(&json).expect("decode memory event");
    assert!(matches!(
        decoded,
        FrontendEvent::MemoryChanged { id, changes, .. }
            if id.0 == "change-1"
                && changes[0].key == "build-gate"
                && changes[0].operation == MemoryOperation::Created
                && changes[0].scope == MemoryChangeScope::Auto
    ));
    // Updated round-trips byte-exact too.
    let updated = serde_json::to_string(&MemoryOperation::Updated).expect("serialize Updated");
    assert_eq!(updated, "\"updated\"");
    let back: MemoryOperation = serde_json::from_str(&updated).expect("decode Updated");
    assert_eq!(back, MemoryOperation::Updated);
}

#[test]
fn test_missing_causality_keeps_event() {
    // A producer that predates the field sends no causality tag. The
    // event must still deliver its changes, read as an earlier turn
    // rather than dropping the whole notice.
    let payload = serde_json::json!({
        "MemoryChanged": {
            "id": "change-3",
            "origin": "auto-memory",
            "changes": [{"key": "older", "operation": "created"}],
        }
    });
    let decoded: FrontendEvent =
        serde_json::from_value(payload).expect("a missing causality must not drop the event");
    assert!(matches!(
        decoded,
        FrontendEvent::MemoryChanged { id, causality, changes, .. }
            if id.0 == "change-3"
                && causality == MemoryChangeCausality::PreviousTurn
                && changes[0].key == "older"
    ));
}

#[test]
fn test_missing_scope_keeps_change() {
    // A producer that predates the scope field sends none. The change
    // must still deliver its key and operation, with the scope unknown
    // rather than a guessed root.
    let payload = serde_json::json!({
        "MemoryChanged": {
            "id": "change-4",
            "origin": "auto-memory",
            "causality": "this-turn",
            "changes": [{"key": "older", "operation": "created"}],
        }
    });
    let decoded: FrontendEvent =
        serde_json::from_value(payload).expect("a missing scope must not drop the change");
    assert!(matches!(
        decoded,
        FrontendEvent::MemoryChanged { changes, .. }
            if changes[0].key == "older"
                && changes[0].scope == MemoryChangeScope::Unknown
    ));
}

#[test]
fn test_unrecognized_scope_keeps_change() {
    // A scope tag a future producer emits that this build does not name
    // must not drop the whole event: the catch-all keeps the change, and
    // the scope reads as unknown so the row names no root.
    let payload = serde_json::json!({
        "MemoryChanged": {
            "id": "change-5",
            "origin": "auto-memory",
            "causality": "this-turn",
            "changes": [{"key": "newer", "operation": "created", "scope": "global"}],
        }
    });
    let decoded: FrontendEvent =
        serde_json::from_value(payload).expect("an unrecognized scope must not drop the event");
    assert!(matches!(
        decoded,
        FrontendEvent::MemoryChanged { changes, .. }
            if changes[0].key == "newer"
                && changes[0].scope == MemoryChangeScope::Unknown
    ));
}

#[test]
fn test_unknown_operation_survives() {
    // A tag a future producer emits that this build does not name lands
    // on Unknown instead of failing the whole change, so the key still
    // reaches the notice rather than being silently dropped.
    let op: MemoryOperation = serde_json::from_str("\"merged\"").expect("decode unknown tag");
    assert_eq!(op, MemoryOperation::Unknown);
    // The full event survives with the key intact.
    let payload = serde_json::json!({
        "MemoryChanged": {
            "id": "change-2",
            "origin": "auto-memory",
            "causality": "this-turn",
            "changes": [{"key": "future", "operation": "merged"}],
        }
    });
    let decoded: FrontendEvent =
        serde_json::from_value(payload).expect("an unknown operation must not drop the event");
    assert!(matches!(
        decoded,
        FrontendEvent::MemoryChanged { id, changes, .. }
            if id.0 == "change-2"
                && changes[0].key == "future"
                && changes[0].operation == MemoryOperation::Unknown
    ));
    // Unknown round-trips as its own kebab-case tag.
    let wire = serde_json::to_string(&MemoryOperation::Unknown).expect("serialize Unknown");
    assert_eq!(wire, "\"unknown\"");
    assert_eq!(
        serde_json::from_str::<MemoryOperation>(&wire).expect("decode unknown"),
        MemoryOperation::Unknown
    );
    // Symmetric forward-compat on origin: an unrecognized producer tag
    // also lands on Unknown rather than dropping the event.
    let unknown_origin: MemoryChangeOrigin =
        serde_json::from_str("\"future-producer\"").expect("decode unknown origin");
    assert_eq!(unknown_origin, MemoryChangeOrigin::Unknown);
    let origin_payload = serde_json::json!({
        "MemoryChanged": {
            "id": "change-3",
            "origin": "future-producer",
            "causality": "this-turn",
            "changes": [{"key": "k", "operation": "created"}],
        }
    });
    let decoded_origin: FrontendEvent =
        serde_json::from_value(origin_payload).expect("an unknown origin must not drop the event");
    assert!(matches!(
        decoded_origin,
        FrontendEvent::MemoryChanged { id, origin, changes, .. }
            if id.0 == "change-3"
                && origin == MemoryChangeOrigin::Unknown
                && changes[0].key == "k"
    ));
    // Symmetric forward-compat on causality: an unrecognized tag also
    // lands on Unknown rather than dropping the notice.
    let unknown_causality: MemoryChangeCausality =
        serde_json::from_str("\"since-restart\"").expect("decode unknown causality");
    assert_eq!(unknown_causality, MemoryChangeCausality::Unknown);
    let causality_payload = serde_json::json!({
        "MemoryChanged": {
            "id": "change-4",
            "origin": "auto-memory",
            "causality": "since-restart",
            "changes": [{"key": "k4", "operation": "created"}],
        }
    });
    let decoded_causality: FrontendEvent = serde_json::from_value(causality_payload)
        .expect("an unknown causality must not drop the event");
    assert!(matches!(
        decoded_causality,
        FrontendEvent::MemoryChanged { id, causality, changes, .. }
            if id.0 == "change-4"
                && causality == MemoryChangeCausality::Unknown
                && changes[0].key == "k4"
    ));
}

#[test]
fn test_commit_wire_compat() {
    let event = FrontendEvent::QueuedInputCommitted {
        inputs: vec![QueuedInput::new("next")],
    };
    let value = serde_json::to_value(&event).unwrap();
    assert!(value.get("QueueConsumed").is_some());
    assert!(value["QueueConsumed"].get("texts").is_some());

    let legacy = serde_json::json!({"QueueConsumed": {"texts": ["next"]}});
    let decoded: FrontendEvent = serde_json::from_value(legacy).unwrap();
    let FrontendEvent::QueuedInputCommitted { inputs } = decoded else {
        panic!("queued input commit");
    };
    assert_eq!(inputs[0].text, "next");
}
