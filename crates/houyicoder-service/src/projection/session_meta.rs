//! Session-sidecar projection. The session-metadata summary is attached by
//! the server's Status handler (the engine snapshot has no access to the
//! sidecar store), so its mapping lives apart from the engine->wire status
//! projection. Pure so it tests without a carrier or a store.

use houyicoder_protocol::frontend::status::{SessionMetaSummary, SessionProvenance};

/// Project the session sidecar to the wire summary so the frontend renders
/// the identity fields (version / name / cwd / provenance) without importing
/// the sidecar store trait. Pure so it tests without a store.
pub(crate) fn project_session_meta(meta: &houyicoder_context::SessionMeta) -> SessionMetaSummary {
    SessionMetaSummary {
        name: meta.name.clone(),
        cwd: meta.cwd.clone(),
        model: meta.model.clone(),
        version: meta.version.clone(),
        provenance: match &meta.provenance {
            houyicoder_context::SessionProvenance::Fresh => SessionProvenance::Fresh,
            houyicoder_context::SessionProvenance::ForkedFrom { from_sid, from_seq } => {
                SessionProvenance::ForkedFrom {
                    from_sid: from_sid.clone(),
                    from_seq: *from_seq,
                }
            }
            houyicoder_context::SessionProvenance::ResumedFromExport { source_session_id } => {
                SessionProvenance::ResumedFromExport {
                    source_session_id: source_session_id.clone(),
                }
            }
            houyicoder_context::SessionProvenance::SpawnedBy {
                parent_session_id,
                subagent_type,
                task_id,
            } => SessionProvenance::SpawnedBy {
                parent_session_id: parent_session_id.clone(),
                subagent_type: subagent_type.clone(),
                task_id: task_id.clone(),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(
        name: Option<&str>,
        provenance: houyicoder_context::SessionProvenance,
    ) -> houyicoder_context::SessionMeta {
        houyicoder_context::SessionMeta {
            name: name.map(str::to_string),
            name_source: houyicoder_context::NameSource::User,
            cwd: "/work/app".to_string(),
            model: "glm-5".to_string(),
            provenance,
            version: env!("CARGO_PKG_VERSION").to_string(),
            created_at: 0,
            child_session_ids: Vec::new(),
        }
    }

    #[test]
    fn test_spawned_by_projects() {
        let p = houyicoder_context::SessionProvenance::SpawnedBy {
            parent_session_id: "parent-1".into(),
            subagent_type: "explore".into(),
            task_id: "task-7".into(),
        };
        let w = project_session_meta(&meta(None, p));
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
    fn test_fresh_provenance_projects() {
        let m = meta(
            Some("fix bug"),
            houyicoder_context::SessionProvenance::Fresh,
        );
        let w = project_session_meta(&m);
        assert_eq!(w.name.as_deref(), Some("fix bug"));
        assert_eq!(w.cwd, "/work/app");
        assert_eq!(w.version, env!("CARGO_PKG_VERSION"));
        assert!(matches!(w.provenance, SessionProvenance::Fresh));
    }

    #[test]
    fn test_forked_provenance_carries_origin() {
        let p = houyicoder_context::SessionProvenance::ForkedFrom {
            from_sid: "sess-aaa".into(),
            from_seq: Some(7),
        };
        let w = project_session_meta(&meta(None, p));
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
        let p = houyicoder_context::SessionProvenance::ResumedFromExport {
            source_session_id: "sess-orig".into(),
        };
        let w = project_session_meta(&meta(None, p));
        match w.provenance {
            SessionProvenance::ResumedFromExport { source_session_id } => {
                assert_eq!(source_session_id, "sess-orig");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
