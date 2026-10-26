//! SessionProvenance and SessionDescriptor serde compatibility tests.

use crate::ids::EventId;

use super::{NameSource, PromptCoverage, PromptRecord, SessionDescriptor, SessionProvenance};

fn event_id(text: &str) -> EventId {
    EventId::from_display_string(text).expect("valid ulid")
}

#[test]
fn test_spawned_by_round_trips() {
    let provenance = SessionProvenance::SpawnedBy {
        parent_session_id: "parent-1".to_string(),
        subagent_type: "explore".to_string(),
        task_id: "task-7".to_string(),
    };
    let json = serde_json::to_string(&provenance).expect("serialize");
    let back: SessionProvenance = serde_json::from_str(&json).expect("deserialize");
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
        first_prompt: Some(PromptRecord {
            text: "fix the login test".to_string(),
            event_id: event_id("01J00000000000000000000000"),
        }),
        last_prompt: None,
        prompt_coverage: PromptCoverage::Complete,
    };
    let json = serde_json::to_string(&descriptor).expect("serialize");
    let back: SessionDescriptor = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.child_session_ids, descriptor.child_session_ids);
}

#[test]
fn test_old_descriptor_defaults_children() {
    let json = r#"{
        "name": null,
        "name_source": "auto",
        "cwd": "/work",
        "model": "stub",
        "provenance": { "kind": "fresh" },
        "version": "0.1.0",
        "created_at": 0
    }"#;
    let descriptor: SessionDescriptor =
        serde_json::from_str(json).expect("deserialize old descriptor");
    assert!(descriptor.child_session_ids.is_empty());
}

#[test]
fn test_old_descriptor_defaults() {
    let json = r#"{
        "name": null,
        "name_source": "auto",
        "cwd": "/work",
        "model": "stub",
        "provenance": { "kind": "fresh" },
        "version": "0.1.0",
        "created_at": 0
    }"#;
    let descriptor: SessionDescriptor =
        serde_json::from_str(json).expect("deserialize old descriptor");
    assert!(descriptor.first_prompt.is_none());
    assert!(descriptor.last_prompt.is_none());
    assert_eq!(
        descriptor.prompt_coverage,
        PromptCoverage::Unknown,
        "an old log was never scanned, so its coverage is unknown"
    );
}

#[test]
fn test_prompt_record_round_trip() {
    let mut descriptor =
        SessionDescriptor::delegated_child(SessionProvenance::Fresh, 0, PromptCoverage::Complete);
    descriptor.first_prompt = Some(PromptRecord {
        text: "first task".to_string(),
        event_id: event_id("01J00000000000000000000000"),
    });
    descriptor.last_prompt = Some(PromptRecord {
        text: "latest task".to_string(),
        event_id: event_id("01J00000000000000000000001"),
    });
    let json = serde_json::to_string(&descriptor).expect("serialize");
    let back: SessionDescriptor = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, descriptor);
}
