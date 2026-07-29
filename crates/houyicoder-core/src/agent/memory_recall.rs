//! agent::memory_recall — the turn-entry memory-recall step plus the
//! surfaced de-dup scan over the projected transcript.
//!
//! Recall fires once per user query, at the run-entry turn boundary (not per
//! model call, which would re-inject across tool round-trips and length
//! recovery). It scans the projected transcript for the surfaced de-dup set,
//! recalls entries the provider has not already surfaced, and appends a
//! durable memory-recall attachment the projection merges into this turn's
//! user message. The system prompt stays byte-frozen across turns (memory is
//! in the message stream, not the prompt) so prompt-cache survives.
//!
//! Compaction gives old memory-recall events the Summarized disposition, so
//! they fold out of the served view. The surfaced scan reads the projection
//! (not the raw append-only log), so a folded memory-recall event is gone
//! from the scanned set — the natural reset that lets entries re-surface
//! post-compress with no provider-side clear. Surfaced tracking reads the
//! projection, not provider state.

use std::collections::HashSet;

use houyicoder_context::{CheckpointManifest, ContextBackend, SessionId, TurnEvent, TurnEventKind};

use super::append::new_event;
use super::{RunError, Runner, context, projection};

/// Collect the keys of memory-recall events that survive in the served view
/// (after applying the manifest) plus their cumulative byte size. Summarized
/// memory-recall events fold out of the view, so their keys drop out of the
/// set and their bytes drop out of the total — the natural reset at the
/// compaction boundary that lets entries re-surface. The de-dup set is what
/// the turn-entry recall passes to the provider so it skips entries the model
/// already sees this turn; the byte total gates cumulative injection (once
/// the session has surfaced enough memory, stop adding more — the most
/// relevant entries are already in context). Scanning the projection (not the
/// raw log) is what makes compaction the reset point: the raw log is
/// append-only, but a folded memory-recall event is gone from the served view.
fn surfaced_memory_scan(
    events: &[TurnEvent],
    manifest: Option<&CheckpointManifest>,
    backend: Option<&dyn ContextBackend>,
) -> (HashSet<String>, usize) {
    let filtered = match manifest {
        Some(m) => projection::apply_manifest(events, m, backend),
        None => events.to_vec(),
    };
    let mut keys = HashSet::new();
    let mut bytes = 0usize;
    for e in filtered.iter() {
        // Read the durable bytes field (recorded at emit) rather than
        // re-measuring text — the recall footprint is a cost dimension the
        // self-evolution loop queries from the log, so the scan reads the
        // same single source. Old logs predate the field (serde default 0):
        // fall back to text.len() so a resumed pre-bytes session still
        // accounts its recall bytes + the cumulative cap still trips.
        if let TurnEventKind::MemoryRecall {
            text,
            keys: ks,
            bytes: b,
            ..
        } = &e.kind
        {
            for k in ks {
                keys.insert(k.clone());
            }
            bytes += if *b > 0 { *b as usize } else { text.len() };
        }
    }
    (keys, bytes)
}

/// Per-session cumulative cap on injected memory bytes. Once the served view
/// has surfaced this much memory, recall stops for the rest of the run (until
/// compact folds the old attachments out, resetting the counter). Bounds a
/// long session where the selector keeps finding distinct files. Roughly
/// seven to eight full injections (the per-turn recall budget is
/// token-sized, each injection lands a few KB of text, and 60KB holds
/// several before it trips).
const MAX_SESSION_BYTES: usize = 60 * 1024;

impl Runner {
    /// Recall relevant memory for the turn about to start and append a durable
    /// memory-recall attachment the projection merges into this turn's user
    /// message. The surfaced de-dup set is scanned from the projected
    /// transcript (memory-recall events still in the served view — Summarized
    /// ones fold out at the compaction boundary, so the set naturally empties
    /// post-compress and entries re-surface with no provider-side clear). The
    /// query is the latest user input in the log. No-op when no memory
    /// provider is wired or recall returns nothing.
    pub(crate) async fn inject_memory_recall(&self, session: SessionId) -> Result<(), RunError> {
        // auto_memory off: skip turn-entry recall entirely. The host flips
        // this via a command (the change lands on this load, no restart);
        // existing memories still surface via the store on a later turn when
        // it is flipped back on.
        if !self.auto_memory.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        let Some(memory) = &self.memory else {
            return Ok(());
        };
        let view = self.store.current_view(session).await?;
        let (surfaced, surfaced_bytes) = surfaced_memory_scan(
            &view.events,
            view.manifest.as_ref(),
            Some(self.store.backend()),
        );
        // Cumulative injection cap: once the session has surfaced enough
        // memory, stop adding more — the most relevant entries are already
        // in context, and further recall only crowds the window. Compact
        // folds old memory-recall events out of the view, which resets this
        // counter naturally (their bytes drop out).
        if surfaced_bytes >= MAX_SESSION_BYTES {
            return Ok(());
        }
        let query = view
            .events
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                TurnEventKind::UserInput { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("");
        // Single-word queries carry too little signal to recall against (a
        // bare "hi" or "thanks" is not a query with enough terms), so skip
        // recall entirely — the turn does not pay a recall + inject for a
        // non-query. Skips single-word prompts.
        if query.split_whitespace().count() <= 1 {
            return Ok(());
        }
        let entries = memory.recall(query, context::MEMORY_RECALL_BUDGET, &surfaced);
        if entries.is_empty() {
            return Ok(());
        }
        let text = context::render_recall_text(&entries);
        let keys: Vec<String> = entries.iter().map(|e| e.key.clone()).collect();
        // Record which keys this recall surfaced so the consolidation dream
        // can later nominate stale entries (low recall hits + old last
        // access). Advisory + best-effort: a write failure re-accumulates
        // next time, never failing the recall path.
        memory.record_recall_hits(&keys);
        let bytes = text.len() as u32;
        self.store
            .append(new_event(
                session,
                TurnEventKind::MemoryRecall { text, keys, bytes },
            ))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{Disposition, EventId, SessionId};

    fn ev(session: SessionId, id: EventId, kind: TurnEventKind) -> TurnEvent {
        TurnEvent {
            id,
            session,
            ts: 0,
            prev_hash: None,
            kind,
        }
    }

    fn recall(keys: &[&str]) -> TurnEventKind {
        let text = "<system-reminder>...</system-reminder>";
        TurnEventKind::MemoryRecall {
            text: text.into(),
            keys: keys.iter().map(|s| s.to_string()).collect(),
            bytes: text.len() as u32,
        }
    }

    fn user(text: &str) -> TurnEventKind {
        TurnEventKind::UserInput { text: text.into() }
    }

    fn assistant(text: &str) -> TurnEventKind {
        TurnEventKind::AssistantMessage {
            text: text.into(),
            thinking: None,
        }
    }

    fn ids(n: usize) -> Vec<EventId> {
        (0..n).map(|_| EventId::new()).collect()
    }

    /// With no manifest (full replay), every memory-recall event is in the
    /// view, so every key is surfaced — recall would skip them all — and the
    /// byte total is the sum of both events' text.
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
        // Both recall events survive in the view, so bytes is twice the
        // fixture's recall text length.
        let one = "<system-reminder>...</system-reminder>".len();
        assert_eq!(bytes, one * 2);
    }

    /// A log predating the durable bytes field deserializes bytes to 0. The
    /// scan falls back to text.len() so a resumed pre-bytes session still
    /// accounts its recall bytes + the cumulative cap still trips — no
    /// silent regression where old recall contributes zero + over-injects.
    #[test]
    fn test_scan_log_falls_back() {
        let s = SessionId::new();
        let text = "<system-reminder>old log recall</system-reminder>";
        let event = TurnEvent {
            id: EventId::new(),
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::MemoryRecall {
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

    /// A memory-recall event folded by a manifest (Summarized) drops out of
    /// the served view, so its key is not surfaced and its bytes are not
    /// counted — the natural reset that lets recall re-surface it
    /// post-compress (both the de-dup set and the cumulative cap reset).
    #[test]
    fn test_scan_excludes_folded() {
        let s = SessionId::new();
        let ids = ids(4);
        // Tail of one assistant turn verbatim; the first memory-recall +
        // its assistant turn are Summarized (folded).
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
        // Only the "kept" recall survives the manifest, so the byte total is
        // one fixture text, not two — the cumulative cap resets at the fold.
        let one = "<system-reminder>...</system-reminder>".len();
        assert_eq!(bytes, one, "a folded recall's bytes drop out of the total");
    }

    /// The real disposition planner folds an old memory-recall event: it takes
    /// the Summarized disposition when it sits before the verbatim tail. This
    /// pins the planner (not a hand-crafted manifest) so a regression that
    /// special-cased memory-recall to Verbatim would fail here. The companion
    /// scan test above then proves a Summarized memory-recall drops out of the
    /// served view — together they are the natural-reset contract.
    #[tokio::test]
    async fn test_planner_folds_recall() {
        use super::super::manifest::{CompressPolicy, HeuristicSummarizer, build_manifest};
        let s = SessionId::new();
        let ids = ids(4);
        // Two assistant turns verbatim (tail_turns=2); the memory-recall +
        // its assistant turn before the boundary are Summarized.
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
