//! SessionProvenance and SessionDescriptor serde compatibility tests.

use crate::{NameSource, SessionDescriptor, SessionProvenance};

#[test]
fn test_spawned_by_round_trips() {
    let provenance = SessionProvenance::SpawnedBy {
        parent_session_id: "parent-1".to_string(),
        subagent_type: "explore".to_string(),
        task_id: "task-7".to_string(),
    };
    let wire = serde_json::to_string(&provenance).expect("serialize");
    let back: SessionProvenance = serde_json::from_str(&wire).expect("deserialize");
    assert_eq!(provenance, back);
}

#[test]
fn test_descriptor_carries_children() {
    let descriptor = SessionDescriptor {
        name: None,
        name_source: NameSource::User,
        cwd: "/work".to_string(),
        model: "stub".to_string(),
        provenance: SessionProvenance::Fresh,
        version: "0.1.0".to_string(),
        created_at: 0,
        child_session_ids: vec!["child-1".to_string(), "child-2".to_string()],
    };
    let wire = serde_json::to_string(&descriptor).expect("serialize");
    let back: SessionDescriptor = serde_json::from_str(&wire).expect("deserialize");
    assert_eq!(back.child_session_ids, descriptor.child_session_ids);
}

#[test]
fn test_old_sidecar_defaults_children() {
    let wire = r#"{
        "name": null,
        "name_source": "auto",
        "cwd": "/work",
        "model": "stub",
        "provenance": { "kind": "fresh" },
        "version": "0.1.0",
        "created_at": 0
    }"#;
    let descriptor: SessionDescriptor =
        serde_json::from_str(wire).expect("deserialize old sidecar");
    assert!(descriptor.child_session_ids.is_empty());
}
