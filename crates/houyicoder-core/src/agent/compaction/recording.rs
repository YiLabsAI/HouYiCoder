//! Durable recording of compaction manifests and events.
//!
//! The manifest is written before CompactionBoundary and Summary. These
//! operations are not transactional; partial failure can leave an unused
//! manifest that a later compaction supersedes.

use houyicoder_api::session::SessionLog;
use houyicoder_context::{
    CheckpointManifest, ContextError, Disposition, EventId, SessionEvent, SessionId,
    SessionLogEntry,
};

/// Result of a compaction recording. Carries the persisted manifest, whether
/// any events were actually folded (no-progress detection), and a count of
/// folded turns.
#[derive(Debug, Clone)]
pub struct RecordedCompaction {
    /// The persisted manifest (also written to the backend).
    pub manifest: CheckpointManifest,
    /// Number of events folded into the summary (Summarized disposition).
    pub folded_count: usize,
    /// True when at least one event was Summarized (progress was made).
    pub made_progress: bool,
}

/// Persist the manifest and its boundary events.
pub(super) async fn record_compaction(
    store: &dyn SessionLog,
    session: SessionId,
    manifest: &CheckpointManifest,
) -> Result<RecordedCompaction, ContextError> {
    let folded_count = manifest
        .plan
        .iter()
        .filter(|g| g.disposition == Disposition::Summarized)
        .map(|g| g.event_ids.len())
        .sum::<usize>();
    let made_progress = folded_count > 0;
    store.write_checkpoint(manifest.clone()).await?;
    if let Some(summary_text) = &manifest.summary {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        store
            .append(SessionLogEntry {
                id: EventId::new(),
                session,
                ts: now,
                prev_hash: None,
                event: SessionEvent::CompactionBoundary {
                    checkpoint: manifest.id,
                },
            })
            .await?;
        store
            .append(SessionLogEntry {
                id: EventId::new(),
                session,
                ts: now,
                prev_hash: None,
                event: SessionEvent::Summary {
                    text: summary_text.clone(),
                },
            })
            .await?;
    }
    Ok(RecordedCompaction {
        manifest: manifest.clone(),
        folded_count,
        made_progress,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::agent::manifest::{CompressPolicy, HeuristicSummarizer, build_manifest};
    use houyicoder_context::{EventId, SessionId};
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_session::SessionStore;

    fn ev(session: SessionId, id: EventId, kind: SessionEvent) -> SessionLogEntry {
        SessionLogEntry {
            id,
            session,
            ts: 0,
            prev_hash: None,
            event: kind,
        }
    }

    fn user(text: &str) -> SessionEvent {
        SessionEvent::UserInput { text: text.into() }
    }

    fn assistant(text: &str) -> SessionEvent {
        SessionEvent::AssistantMessage {
            text: text.into(),
            thinking: None,
        }
    }

    fn ids(n: usize) -> Vec<EventId> {
        (0..n).map(|_| EventId::new()).collect()
    }

    async fn build_and_record(
        store: &SessionStore,
        session: SessionId,
        events: &[SessionLogEntry],
        policy: &CompressPolicy,
    ) -> RecordedCompaction {
        let manifest = build_manifest(events, policy, &HeuristicSummarizer, None).await;
        record_compaction(store, session, &manifest).await.unwrap()
    }

    #[tokio::test]
    async fn test_record_writes_checkpoint_events() {
        let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let s = SessionId::new();
        let ids = ids(4);
        let events = vec![
            ev(s, ids[0], user("do work")),
            ev(s, ids[1], assistant("old response")),
            ev(s, ids[2], assistant("middle")),
            ev(s, ids[3], assistant("latest")),
        ];
        for e in &events {
            store.append(e.clone()).await.unwrap();
        }
        let policy = CompressPolicy {
            tail_turns: 1,
            preserve_recent_tokens: 0,
            large_output_bytes: 0,
        };
        let result = build_and_record(&store, s, &events, &policy).await;
        assert!(result.made_progress, "must fold some events");
        assert!(result.folded_count > 0);
        let back = store.read_checkpoint(result.manifest.id).await.unwrap();
        assert_eq!(back.summary, result.manifest.summary);
        assert_eq!(back.plan.len(), result.manifest.plan.len());
        let replay = store.replay(s).await.unwrap();
        let boundary_count = replay
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::CompactionBoundary { .. }))
            .count();
        assert_eq!(boundary_count, 1, "one compaction boundary");
        let summary_count = replay
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::Summary { .. }))
            .count();
        assert_eq!(summary_count, 1, "one summary event");
    }

    #[tokio::test]
    async fn test_no_progress_keeps_all() {
        let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let s = SessionId::new();
        let ids = ids(2);
        let events = vec![ev(s, ids[0], user("task")), ev(s, ids[1], assistant("a1"))];
        let policy = CompressPolicy {
            tail_turns: 4,
            preserve_recent_tokens: 0,
            large_output_bytes: 0,
        };
        let result = build_and_record(&store, s, &events, &policy).await;
        assert!(!result.made_progress, "all verbatim = no progress");
        assert_eq!(result.folded_count, 0);
        assert!(result.manifest.summary.is_none());
    }

    #[tokio::test]
    async fn test_record_empty_events() {
        let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let s = SessionId::new();
        let policy = CompressPolicy::default();
        let result = build_and_record(&store, s, &[], &policy).await;
        assert!(!result.made_progress);
        assert!(result.manifest.plan.is_empty());
        assert!(result.manifest.summary.is_none());
        let replay = store.replay(s).await.unwrap();
        assert!(replay.is_empty(), "no boundary/summary for empty events");
    }

    #[tokio::test]
    async fn test_record_checkpoint_round_trips() {
        let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let s = SessionId::new();
        let ids = ids(5);
        let events = vec![
            ev(s, ids[0], user("start")),
            ev(s, ids[1], assistant("a1")),
            ev(s, ids[2], assistant("a2")),
            ev(s, ids[3], assistant("a3")),
            ev(s, ids[4], assistant("latest")),
        ];
        for e in &events {
            store.append(e.clone()).await.unwrap();
        }
        let policy = CompressPolicy {
            tail_turns: 1,
            preserve_recent_tokens: 0,
            large_output_bytes: 0,
        };
        let result = build_and_record(&store, s, &events, &policy).await;
        let id = result.manifest.id;
        let back = store.read_checkpoint(id).await.unwrap();
        assert_eq!(back.id, id);
        assert_eq!(back.session, s);
        assert_eq!(back.summary, result.manifest.summary);
        assert_eq!(back.plan, result.manifest.plan);
        let list = store.list_checkpoints(s).await.unwrap();
        assert!(list.contains(&id));
    }
}
