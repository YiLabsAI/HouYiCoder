//! Recalls memory once per user query.
//!
//! Injected keys come from the assembled context, so compaction naturally
//! permits relevant memories to appear again. A confident lexical selection
//! injects synchronously. On a weak lexical signal, a session's initial turn
//! awaits semantic selection under a short deadline so step zero receives
//! relevant memories, whereas later turns run the semantic stage alongside
//! the main model call and append its recall on completion.

use std::collections::HashSet;
use std::sync::Arc;

use houyicoder_api::memory::{MemoryProvider, MemoryReranker};
use houyicoder_api::session::SessionLog;
use houyicoder_context::{
    CheckpointManifest, ContextBackend, MemoryRankHit, SessionEvent, SessionId, SessionLogEntry,
};
use tokio_util::sync::CancellationToken;

use super::gates::MemoryGates;
use super::selector::{
    INITIAL_STEP_RERANK_TIMEOUT, RERANK_TIMEOUT, RecallFallback, RecallTasks, RecallTelemetry,
    RecallVerdict, classify, lexical_keys, materialize, run_semantic_selection,
};
use crate::agent::RunError;
use crate::agent::append::new_event;
use crate::agent::context;
use crate::agent::selection::apply_manifest;
use crate::observability::tool_failure_reason;

/// Maximum memory bytes retained in the active context.
pub(super) const MAX_SESSION_BYTES: usize = 60 * 1024;

/// Re-inject after compaction, extracting file stems and salient keywords
/// from the latest summary. This scopes recall to the work that was folded
/// without flooding the query with template prose or raw summary text.
pub(crate) async fn recall_after_compaction(
    store: &Arc<dyn SessionLog>,
    provider: Option<&Arc<dyn MemoryProvider>>,
    reranker: Option<&Arc<dyn MemoryReranker>>,
    tasks: &RecallTasks,
    gates: &MemoryGates,
    session: SessionId,
) -> Result<(), RunError> {
    let view = store.current_view(session).await?;
    let summary_text = view
        .events
        .iter()
        .rev()
        .find_map(|entry| match &entry.event {
            SessionEvent::Summary { text } => Some(text.as_str()),
            _ => None,
        });
    let hint = summary_text.map(extract_summary_keywords);
    execute_recall(
        store,
        provider,
        reranker,
        tasks,
        gates,
        session,
        hint.as_deref(),
        None,
    )
    .await
}

/// Extract salient file stems and active todo keywords from summary text.
/// Prefers the authoritative Files touched: and Todo snapshot: sections
/// produced by the compaction backbone over conversational summary text.
fn extract_summary_keywords(summary: &str) -> String {
    let mut words = Vec::new();
    let mut in_files = false;
    let mut in_todos = false;

    for line in summary.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Files touched:") {
            in_files = true;
            in_todos = false;
            continue;
        } else if trimmed.starts_with("Todo snapshot:") {
            in_todos = true;
            in_files = false;
            continue;
        } else if trimmed.starts_with("Test results:")
            || trimmed.starts_with("Derivation:")
            || trimmed.starts_with("---")
        {
            in_files = false;
            in_todos = false;
        }

        if in_files && let Some(path) = trimmed.strip_prefix("- ") {
            let stem = path.rsplit(['/', '\\']).next().unwrap_or(path);
            let clean = stem.split('.').next().unwrap_or(stem);
            if clean.len() >= 3 && !words.contains(&clean) {
                words.push(clean);
                if words.len() >= 8 {
                    break;
                }
            }
        } else if in_todos && (trimmed.contains("[in_progress]") || trimmed.contains("[pending]")) {
            for token in trimmed.split_whitespace() {
                let clean = token.trim_matches(|c: char| !c.is_alphanumeric());
                if clean.len() >= 4 && !words.contains(&clean) {
                    words.push(clean);
                    if words.len() >= 8 {
                        break;
                    }
                }
            }
        }
    }

    // Fallback: if no structured backbone sections were found, take stems from any list item.
    if words.is_empty() {
        for line in summary.lines() {
            let trimmed = line.trim();
            if let Some(path) = trimmed.strip_prefix("- ") {
                let stem = path.rsplit(['/', '\\']).next().unwrap_or(path);
                let clean = stem.split('.').next().unwrap_or(stem);
                if clean.len() >= 3 && !words.contains(&clean) {
                    words.push(clean);
                    if words.len() >= 8 {
                        break;
                    }
                }
            }
        }
    }

    words.join(" ")
}

pub(crate) async fn recall(
    store: &Arc<dyn SessionLog>,
    provider: Option<&Arc<dyn MemoryProvider>>,
    reranker: Option<&Arc<dyn MemoryReranker>>,
    tasks: &RecallTasks,
    gates: &MemoryGates,
    session: SessionId,
    cancel: Option<&CancellationToken>,
) -> Result<(), RunError> {
    execute_recall(
        store, provider, reranker, tasks, gates, session, None, cancel,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_recall(
    store: &Arc<dyn SessionLog>,
    provider: Option<&Arc<dyn MemoryProvider>>,
    reranker: Option<&Arc<dyn MemoryReranker>>,
    tasks: &RecallTasks,
    gates: &MemoryGates,
    session: SessionId,
    extra_query: Option<&str>,
    cancel: Option<&CancellationToken>,
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
    let (mut injected_keys, injected_key_bytes) =
        injected_key_scan(&view.events, view.manifest.as_ref(), Some(store.backend()));
    if injected_key_bytes >= MAX_SESSION_BYTES {
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
    // Candidates a still-running selection holds are treated as already injected,
    // so a second recall in the same turn neither re-selects nor
    // double-injects them.
    injected_keys.extend(reserved);
    // A key this session already wrote or read is in the model's view; a
    // recall of it would spend the injection budget on text the model has
    // already seen.
    injected_keys.extend(session_written_keys(&view.events));
    let last_user = view
        .events
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            SessionEvent::UserInput { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("");
    let query = match extra_query {
        Some(extra) => format!("{last_user} {extra}"),
        None => last_user.to_string(),
    };
    // The signal gate lives in the provider. A query with no word content
    // ranks nothing; a query the tokenizer cannot split (one CJK char) still
    // offers its scanned rows to the semantic stage at score zero.
    let scored = memory.rank_candidates(&query, &injected_keys);
    let verdict = classify(&scored);
    let lexical_hits = scored.iter().filter(|h| h.score > 0).count();
    let is_initial_turn = extra_query.is_none()
        && !view
            .events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::AssistantMessage { .. }));
    if verdict.needs_rerank()
        && let Some(reranker) = reranker
    {
        if is_initial_turn {
            return recall_initial_step(
                store,
                memory,
                reranker,
                query,
                scored,
                lexical_hits,
                session,
                cancel,
            )
            .await;
        }
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

#[expect(
    clippy::too_many_arguments,
    reason = "forwards resolved recall state to the initial-turn branch"
)]
async fn recall_initial_step(
    store: &Arc<dyn SessionLog>,
    memory: &Arc<dyn MemoryProvider>,
    reranker: &Arc<dyn MemoryReranker>,
    query: String,
    scored: Vec<MemoryRankHit>,
    lexical_hits: usize,
    session: SessionId,
    cancel: Option<&CancellationToken>,
) -> Result<(), RunError> {
    let selection = run_semantic_selection(
        Arc::clone(reranker),
        query,
        &scored,
        INITIAL_STEP_RERANK_TIMEOUT,
    );
    let (keys, fallback) = match cancel {
        Some(token) => tokio::select! {
            biased;
            () = token.cancelled() => return Ok(()),
            res = selection => res,
        },
        None => selection.await,
    };
    let (injected, bytes) = inject_selected(store, memory, session, &keys).await?;
    RecallTelemetry {
        candidate_count: scored.len(),
        lexical_hits,
        rerank_triggered: true,
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
        let (keys, fallback) =
            run_semantic_selection(reranker, query, &scored, RERANK_TIMEOUT).await;
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

/// Memory tool names whose key the model has already seen. A successful
/// write or read of a key puts its text in the model's view, so a recall of
/// that key only repeats it. Both canonical memory_* names and legacy aliases
/// are recognized so resumed historical sessions correctly detect seen keys.
const SEEN_KEY_TOOLS: [&str; 4] = ["memory_write", "memory_read", "save_memory", "show_memory"];

/// Keys this session successfully wrote or read. A call whose result failed
/// or has not completed yet is excluded, so only completed, successful
/// tool results count as seen.
pub(super) fn session_written_keys(events: &[SessionLogEntry]) -> HashSet<String> {
    let succeeded: HashSet<&str> = events
        .iter()
        .filter_map(|entry| match &entry.event {
            SessionEvent::ToolResult {
                call_id, output, ..
            } if tool_failure_reason(output).is_none() => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    events
        .iter()
        .filter_map(|entry| match &entry.event {
            SessionEvent::ToolCall {
                call_id,
                tool,
                input,
            } if SEEN_KEY_TOOLS.contains(&tool.as_str())
                && succeeded.contains(call_id.as_str()) =>
            {
                input
                    .get("key")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            }
            _ => None,
        })
        .collect()
}

/// Collect the keys already in the assembled context and their bytes.
pub(super) fn injected_key_scan(
    events: &[SessionLogEntry],
    manifest: Option<&CheckpointManifest>,
    backend: Option<&dyn ContextBackend>,
) -> (HashSet<String>, usize) {
    let filtered = match manifest {
        Some(m) => apply_manifest(events, m, backend),
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
mod tests;
