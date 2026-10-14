//! Tests for this module.

use super::*;
use houyicoder_context::{NameSource, SessionProvenance as ContextProvenance};

fn descriptor(
    name: Option<&str>,
    provenance: ContextProvenance,
) -> houyicoder_context::SessionDescriptor {
    houyicoder_context::SessionDescriptor {
        name: name.map(str::to_string),
        name_source: NameSource::User,
        cwd: "/work/app".to_string(),
        model: "glm-5".to_string(),
        provenance,
        version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: 0,
        child_session_ids: Vec::new(),
    }
}

#[test]
fn test_spawned_by_carries_parent() {
    let p = ContextProvenance::SpawnedBy {
        parent_session_id: "parent-1".into(),
        subagent_type: "explore".into(),
        task_id: "task-7".into(),
    };
    let w = map_session_descriptor(&descriptor(None, p));
    match w.provenance {
        SessionProvenance::SpawnedBy {
            parent_session_id,
            subagent_type,
            task_id,
        } => {
            assert_eq!(parent_session_id, "parent-1");
            assert_eq!(subagent_type, "explore");
            assert_eq!(task_id, "task-7");
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn test_fresh_provenance_carries_name() {
    let source = descriptor(Some("fix bug"), ContextProvenance::Fresh);
    let w = map_session_descriptor(&source);
    assert_eq!(w.name.as_deref(), Some("fix bug"));
    assert_eq!(w.cwd, "/work/app");
    assert_eq!(w.version, env!("CARGO_PKG_VERSION"));
    assert!(matches!(w.provenance, SessionProvenance::Fresh));
}

#[test]
fn test_forked_provenance_carries_origin() {
    let p = ContextProvenance::ForkedFrom {
        from_sid: "sess-aaa".into(),
        from_seq: Some(7),
    };
    let w = map_session_descriptor(&descriptor(None, p));
    match w.provenance {
        SessionProvenance::ForkedFrom { from_sid, from_seq } => {
            assert_eq!(from_sid, "sess-aaa");
            assert_eq!(from_seq, Some(7));
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn test_resumed_provenance_carries_source() {
    let p = ContextProvenance::ResumedFromExport {
        source_session_id: "sess-orig".into(),
    };
    let w = map_session_descriptor(&descriptor(None, p));
    match w.provenance {
        SessionProvenance::ResumedFromExport { source_session_id } => {
            assert_eq!(source_session_id, "sess-orig");
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
