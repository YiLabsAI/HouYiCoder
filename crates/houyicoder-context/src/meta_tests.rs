//! SessionProvenance + SessionMeta serde round-trips for the spawn lineage.

use crate::{NameSource, SessionMeta, SessionProvenance};

/// SpawnedBy provenance round-trips through the sidecar wire: a child session
/// records its parent, the agent type, and the task that launched it, and a
/// read-back preserves all three.
#[test]
fn test_spawned_by_round_trips() {
    let p = SessionProvenance::SpawnedBy {
        parent_session_id: "parent-1".to_string(),
        subagent_type: "explore".to_string(),
        task_id: "task-7".to_string(),
    };
    let wire = serde_json::to_string(&p).expect("serialize");
    let back: SessionProvenance = serde_json::from_str(&wire).expect("deserialize");
    assert_eq!(p, back);
}

/// A parent session's child list round-trips through the sidecar, so a
/// parent resumed later can still enumerate the children it spawned.
#[test]
fn test_meta_carries_children() {
    let meta = SessionMeta {
        name: None,
        name_source: NameSource::User,
        cwd: "/work".to_string(),
        model: "stub".to_string(),
        provenance: SessionProvenance::Fresh,
        version: "0.1.0".to_string(),
        created_at: 0,
        child_session_ids: vec!["child-1".to_string(), "child-2".to_string()],
    };
    let wire = serde_json::to_string(&meta).expect("serialize");
    let back: SessionMeta = serde_json::from_str(&wire).expect("deserialize");
    assert_eq!(back.child_session_ids, meta.child_session_ids);
}
