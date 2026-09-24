//! Recalls memory once per user query.
//!
//! Surfaced keys come from the projected context, so compaction naturally
//! permits relevant memories to appear again. A confident lexical selection
//! injects synchronously; a weak signal runs the semantic stage alongside
//! the main model call and appends its recall on completion, so the first
//! token never waits on a model-backed selection.

use std::collections::HashSet;
use std::sync::Arc;

use houyicoder_api::memory::{MemoryProvider, MemoryReranker};
use houyicoder_api::session::SessionLog;
use houyicoder_context::{
    CheckpointManifest, ContextBackend, MemoryRankHit, SessionEvent, SessionId, SessionLogEntry,
};

use super::gates::MemoryGates;
use super::selector::{
    RecallFallback, RecallTasks, RecallTelemetry, RecallVerdict, classify, lexical_keys,
    materialize, run_semantic_selection,
};
use crate::agent::RunError;
use crate::agent::append::new_event;
use crate::agent::context;

/// Maximum memory bytes retained in the active context.
const MAX_SESSION_BYTES: usize = 60 * 1024;

/// Append relevant memories that are not already active in the context.
pub(crate) async fn recall(
    store: &Arc<dyn SessionLog>,
    provider: Option<&Arc<dyn MemoryProvider>>,
    reranker: Option<&Arc<dyn MemoryReranker>>,
    tasks: &RecallTasks,
    gates: &MemoryGates,
    session: SessionId,
) -> Result<(), RunError> {
    if !gates.auto_memory_enabled() {
        return Ok(());
    }
    let Some(memory) = provider else {
        return Ok(());
    };
    // The reservation is sampled before the view snapshot. A selection that
    // finishes after this point stays reserved here, and one that finished
    // earlier has its append in the snapshot, so no key escapes both nets.
    let reserved = tasks.pending_keys(session);
    let view = store.current_view(session).await?;
    let (mut surfaced, surfaced_bytes) =
        surfaced_memory_scan(&view.events, view.manifest.as_ref(), Some(store.backend()));
    if surfaced_bytes >= MAX_SESSION_BYTES {
        RecallTelemetry {
            candidate_count: 0,
            lexical_hits: 0,
            rerank_triggered: false,
            selected_keys: Vec::new(),
            fallback: Some(RecallFallback::ByteCap),
            injected_bytes: 0,
        }
        .log();
        return Ok(());
    }
    // Candidates a still-running selection holds are treated as surfaced,
    // so a second recall in the same turn neither re-selects nor
    // double-injects them.
    surfaced.extend(reserved);
    let query = view
        .events
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            SessionEvent::UserInput { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("");
    // The signal gate lives in the provider. A query with no word content
    // ranks nothing; a query the tokenizer cannot split (one CJK char) still
    // offers its scanned rows to the semantic stage at score zero.
    let scored = memory.rank_candidates(query, &surfaced);
    let verdict = classify(&scored);
    let lexical_hits = scored.iter().filter(|h| h.score > 0).count();
    if verdict.needs_rerank()
        && let Some(reranker) = reranker
    {
        spawn_selection(
            Arc::clone(store),
            Arc::clone(memory),
            Arc::clone(reranker),
            query.to_string(),
            scored,
            lexical_hits,
            session,
            tasks,
        );
        return Ok(());
    }
    // Synchronous when the lexical selection is confident, or when no
    // reranker is installed and the deterministic answer is the best
    // available.
    let (keys, fallback) = match verdict {
        RecallVerdict::NoCandidates => (Vec::new(), None),
        RecallVerdict::Confident => (lexical_keys(&scored), None),
        _ => {
            let keys = lexical_keys(&scored);
            let fallback = if keys.is_empty() {
                RecallFallback::NoSignal
            } else {
                RecallFallback::NoReranker
            };
            (keys, Some(fallback))
        }
    };
    let (injected, bytes) = inject_selected(store, memory, session, &keys).await?;
    RecallTelemetry {
        candidate_count: scored.len(),
        lexical_hits,
        rerank_triggered: false,
        selected_keys: injected,
        fallback,
        injected_bytes: bytes,
    }
    .log();
    Ok(())
}

/// Run the semantic selection beside the main model call and append its
/// recall on completion. The per-step view assembly picks the append up at
/// the next model step of the same turn.
#[allow(clippy::too_many_arguments)]
fn spawn_selection(
    store: Arc<dyn SessionLog>,
    memory: Arc<dyn MemoryProvider>,
    reranker: Arc<dyn MemoryReranker>,
    query: String,
    scored: Vec<MemoryRankHit>,
    lexical_hits: usize,
    session: SessionId,
    tasks: &RecallTasks,
) {
    let candidate_count = scored.len();
    let reserved: HashSet<String> = scored.iter().map(|h| h.key.clone()).collect();
    let handle = tokio::spawn(async move {
        let (keys, fallback) = run_semantic_selection(reranker, query, &scored).await;
        let (injected, bytes) = match inject_selected(&store, &memory, session, &keys).await {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!("background recall injection failed: {error}");
                (Vec::new(), 0)
            }
        };
        RecallTelemetry {
            candidate_count,
            lexical_hits,
            rerank_triggered: true,
            selected_keys: injected,
            fallback,
            injected_bytes: bytes,
        }
        .log();
    });
    tasks.track(handle, session, reserved);
}

/// Read the selected bodies under the budget and append the recall event.
/// Returns the keys actually injected and the attachment size in bytes.
async fn inject_selected(
    store: &Arc<dyn SessionLog>,
    memory: &Arc<dyn MemoryProvider>,
    session: SessionId,
    keys: &[String],
) -> Result<(Vec<String>, usize), RunError> {
    let entries = materialize(memory.as_ref(), keys, context::MEMORY_RECALL_BUDGET);
    if entries.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let text = context::render_recall_text(&entries);
    let injected: Vec<String> = entries.iter().map(|e| e.key.clone()).collect();
    memory.record_recall_hits(&injected);
    let bytes = text.len();
    store
        .append(new_event(
            session,
            SessionEvent::MemoryRecall {
                text,
                keys: injected.clone(),
                bytes: bytes as u32,
            },
        ))
        .await?;
    Ok((injected, bytes))
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
#[path = "recall_tests.rs"]
mod recall_tests;
