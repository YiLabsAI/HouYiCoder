//! Recalls memory once per user query.
//!
//! Surfaced keys come from the projected context, so compaction naturally
//! permits relevant memories to appear again.

use std::collections::HashSet;
use std::sync::Arc;

use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_context::{
    CheckpointManifest, ContextBackend, SessionEvent, SessionId, SessionLogEntry,
};

use super::gates::MemoryGates;
use crate::agent::append::new_event;
use crate::agent::context;

/// Maximum memory bytes retained in the active context.
const MAX_SESSION_BYTES: usize = 60 * 1024;

/// Append relevant memories that are not already active in the context.
pub(crate) async fn recall(
    store: &Arc<dyn SessionLog>,
    provider: Option<&Arc<dyn MemoryProvider>>,
    gates: &MemoryGates,
    session: SessionId,
) -> Result<(), crate::agent::RunError> {
    if !gates.auto_memory_enabled() {
        return Ok(());
    }
    let Some(memory) = provider else {
        return Ok(());
    };
    let view = store.current_view(session).await?;
    let (surfaced, surfaced_bytes) =
        surfaced_memory_scan(&view.events, view.manifest.as_ref(), Some(store.backend()));
    if surfaced_bytes >= MAX_SESSION_BYTES {
        return Ok(());
    }
    let query = view
        .events
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            SessionEvent::UserInput { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("");
    // Single-word queries carry too little signal to recall against.
    if query.split_whitespace().count() <= 1 {
        return Ok(());
    }
    let entries = memory.recall(query, context::MEMORY_RECALL_BUDGET, &surfaced);
    if entries.is_empty() {
        return Ok(());
    }
    let text = context::render_recall_text(&entries);
    let keys: Vec<String> = entries.iter().map(|e| e.key.clone()).collect();
    memory.record_recall_hits(&keys);
    let bytes = text.len() as u32;
    store
        .append(new_event(
            session,
            SessionEvent::MemoryRecall { text, keys, bytes },
        ))
        .await?;
    Ok(())
}

/// Collect surfaced keys and bytes from the projected context.
fn surfaced_memory_scan(
    events: &[SessionLogEntry],
    manifest: Option<&CheckpointManifest>,
    backend: Option<&dyn ContextBackend>,
) -> (HashSet<String>, usize) {
    let filtered = match manifest {
        Some(m) => crate::agent::selection::apply_manifest(events, m, backend),
        None => events.to_vec(),
    };
    let mut keys = HashSet::new();
    let mut bytes = 0usize;
    for e in filtered.iter() {
        // Older events use zero and require measuring their stored text.
        if let SessionEvent::MemoryRecall {
            text,
            keys: ks,
            bytes: b,
            ..
        } = &e.event
        {
            for k in ks {
                keys.insert(k.clone());
            }
            bytes += if *b > 0 { *b as usize } else { text.len() };
        }
    }
    (keys, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{Disposition, EventId, SessionId};

    fn ev(session: SessionId, id: EventId, kind: SessionEvent) -> SessionLogEntry {
        SessionLogEntry {
            id,
            session,
            ts: 0,
            prev_hash: None,
            event: kind,
        }
    }

    fn recall(keys: &[&str]) -> SessionEvent {
        let text = "<system-reminder>...</system-reminder>";
        SessionEvent::MemoryRecall {
            text: text.into(),
            keys: keys.iter().map(|s| s.to_string()).collect(),
            bytes: text.len() as u32,
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

    #[test]
    fn test_scan_collects_all() {
        let s = SessionId::new();
        let ids = ids(3);
        let events = vec![
            ev(s, ids[0], recall(&["alpha"])),
            ev(s, ids[1], recall(&["bravo", "charlie"])),
            ev(s, ids[2], user("query")),
        ];
        let (keys, bytes) = surfaced_memory_scan(&events, None, None);
        assert!(keys.contains("alpha"));
        assert!(keys.contains("bravo"));
        assert!(keys.contains("charlie"));
        assert_eq!(keys.len(), 3);
        let one = "<system-reminder>...</system-reminder>".len();
        assert_eq!(bytes, one * 2);
    }

    #[test]
    fn test_scan_log_falls_back() {
        let s = SessionId::new();
        let text = "<system-reminder>old log recall</system-reminder>";
        let event = SessionLogEntry {
            id: EventId::new(),
            session: s,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::MemoryRecall {
                text: text.into(),
                keys: vec!["old".into()],
                bytes: 0,
            },
        };
        let (keys, bytes) = surfaced_memory_scan(&[event], None, None);
        assert!(keys.contains("old"));
        assert_eq!(
            bytes,
            text.len(),
            "old log (bytes=0) falls back to text.len()"
        );
    }

    #[test]
    fn test_scan_excludes_folded() {
        let s = SessionId::new();
        let ids = ids(4);
        let events = vec![
            ev(s, ids[0], recall(&["folded"])),
            ev(s, ids[1], assistant("old")),
            ev(s, ids[2], assistant("boundary")),
            ev(s, ids[3], recall(&["kept"])),
        ];
        let manifest = {
            use houyicoder_context::{CheckpointId, CheckpointManifest, Disposition, TurnGroup};
            CheckpointManifest {
                id: CheckpointId::new(),
                session: s,
                last_event: ids[3],
                summary: Some("summary".into()),
                plan: vec![
                    TurnGroup {
                        turn_id: ids[0],
                        disposition: Disposition::Summarized,
                        event_ids: vec![ids[0], ids[1]],
                    },
                    TurnGroup {
                        turn_id: ids[2],
                        disposition: Disposition::Verbatim,
                        event_ids: vec![ids[2], ids[3]],
                    },
                ],
                ts: 0,
            }
        };
        let (keys, bytes) = surfaced_memory_scan(&events, Some(&manifest), None);
        assert!(
            !keys.contains("folded"),
            "a Summarized memory-recall must drop out of the surfaced set"
        );
        assert!(
            keys.contains("kept"),
            "a Verbatim memory-recall must stay in the surfaced set"
        );
        let one = "<system-reminder>...</system-reminder>".len();
        assert_eq!(bytes, one, "a folded recall's bytes drop out of the total");
    }

    #[tokio::test]
    async fn test_planner_folds_recall() {
        use crate::agent::manifest::{CompressPolicy, HeuristicSummarizer, build_manifest};
        let s = SessionId::new();
        let ids = ids(4);
        let events = vec![
            ev(s, ids[0], recall(&["folded"])),
            ev(s, ids[1], assistant("old turn")),
            ev(s, ids[2], assistant("boundary turn")),
            ev(s, ids[3], assistant("latest turn")),
        ];
        let policy = CompressPolicy {
            tail_turns: 2,
            preserve_recent_tokens: 0,
            large_output_bytes: 0,
        };
        let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
        let disp = manifest
            .plan
            .iter()
            .find(|g| g.event_ids.contains(&ids[0]))
            .map(|g| g.disposition)
            .expect("memory-recall event must be in the plan");
        assert_eq!(
            disp,
            Disposition::Summarized,
            "an old memory-recall must take Summarized so compaction folds it"
        );
    }
}
