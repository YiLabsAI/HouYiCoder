//! agent::lifecycle — the Compress stage runtime + the LLM summarizer.
//!
//! The Compress stage is the "don't brick" safety net for long-running sessions
//! that approach the context window. When the served view exceeds the
//! pre-flight threshold (95% of the window) or the provider returns
//! ContextOverflow, the loop calls compress_session to fold older events into
//! a summary, persist a CheckpointManifest, and append CompactionBoundary +
//! Summary events so the next select() yields a smaller served window.
//!
//! LlmSummarizer calls the provider to produce a real summary, chunked by
//! assistant turns to prevent the summarizer itself from overflowing. When the
//! provider is unavailable or fails, it falls back to the heuristic summarizer
//! so the pipeline never bricks.

use std::sync::Arc;

use houyicoder_api::provider::ModelProvider;
use houyicoder_async::PFut;
use houyicoder_context::{
    CheckpointManifest, Disposition, EventId, MemoryEntry, MemorySource, SessionId, TurnEvent,
    TurnEventKind,
};
use houyicoder_protocol::llm::{CompletionRequest, CompletionResponse, ModelSettings, OutputItem};

use super::manifest::{
    CompressPolicy, HeuristicSummarizer, SummarizeError, Summarizer, build_manifest,
};
use super::turn_group;

/// The instruction sent to the provider when summarizing a folded span. A
/// four-section structure (Primary intent / Current work / Pending / Key
/// files-decisions) directs the summarizer to surface what a resumed run
/// needs most, instead of a free-form recap that buries the active task.
/// Kept short: the instruction is a few hundred tokens against a 50k chunk
/// budget, so it does not eat the summarizer's own room.
const SUMMARIZER_INSTRUCTION: &str = "You are a helpful AI assistant tasked with \
summarizing the conversation below. Produce a plain-text summary with these \
sections, each a labeled paragraph (skip a section if it is empty):\n\
1. Primary intent: what the user asked for and the high-level goal of the session.\n\
2. Current work: the active task right before this summary — the last tool calls, \
the last result, and what was about to happen next.\n\
3. Pending: outstanding tasks, unanswered questions, and next steps the user expects.\n\
4. Key files and decisions: files read, changed, or written (concrete paths) and the \
important decisions or constraints established. Use paths and values, not prose.";

/// Default per-chunk token budget for the LLM summarizer. The folded span is
/// split by assistant turns so each chunk fits well under the context window,
/// preventing the summarizer from itself triggering overflow.
const DEFAULT_CHUNK_TOKEN_LIMIT: usize = 50_000;

/// An LLM-backed summarizer. Calls the provider with chunked input (split by
/// assistant turns) to produce a summary of the folded span. Falls back to the
/// heuristic summarizer when the provider fails or returns no usable text.
pub struct LlmSummarizer {
    provider: Arc<dyn ModelProvider>,
    model: String,
    chunk_token_limit: usize,
}

impl LlmSummarizer {
    /// Construct an LLM summarizer over the given provider. The model id is the
    /// one the main loop uses; chunk_token_limit caps each summarization chunk.
    pub fn new(provider: Arc<dyn ModelProvider>, model: String) -> Self {
        Self {
            provider,
            model,
            chunk_token_limit: DEFAULT_CHUNK_TOKEN_LIMIT,
        }
    }

    /// Override the per-chunk token budget (tests use a small value to force
    /// multi-chunk splitting without sizing a real conversation).
    pub fn with_chunk_limit(mut self, limit: usize) -> Self {
        self.chunk_token_limit = limit;
        self
    }
}

impl Summarizer for LlmSummarizer {
    fn summarize<'a>(
        &'a self,
        events: &'a [TurnEvent],
        custom_instructions: Option<&'a str>,
    ) -> PFut<'a, Result<String, SummarizeError>> {
        if events.is_empty() {
            return Box::pin(async move { Err(SummarizeError::Empty) });
        }

        let chunks = chunk_by_assistant_turn(events, self.chunk_token_limit);
        let provider = self.provider.clone();
        let model = self.model.clone();

        Box::pin(async move {
            // Merge custom instructions (from a PreCompact hook or a future
            // /compact argument) into the base instruction so the summarizer
            // can be steered without a separate prompt path. Hook output
            // appends to the base compact prompt.
            let instructions = match custom_instructions {
                Some(extra) if !extra.is_empty() => {
                    format!("{SUMMARIZER_INSTRUCTION}\n\n{extra}")
                }
                _ => SUMMARIZER_INSTRUCTION.to_string(),
            };
            let mut summaries: Vec<String> = Vec::with_capacity(chunks.len());
            for chunk in &chunks {
                let input = turn_group::project_input_items(chunk, None);
                if input.is_empty() {
                    continue;
                }
                let req = CompletionRequest {
                    model: model.clone(),
                    instructions: instructions.clone(),
                    input,
                    tools: Vec::new(),
                    settings: ModelSettings::default(),
                    cache_breakpoints: Vec::new(),
                };
                match provider.complete(req).await {
                    Ok(resp) => match extract_text(&resp) {
                        Some(text) if !text.is_empty() => summaries.push(text),
                        _ => {
                            // No text in response: fall back to heuristic.
                            let fallback = HeuristicSummarizer.summarize(events, None).await;
                            return fallback.map_err(|_| {
                                SummarizeError::LlmFailed("no text in provider response".into())
                            });
                        }
                    },
                    Err(e) => {
                        // Provider error: fall back to heuristic.
                        let fallback = HeuristicSummarizer.summarize(events, None).await;
                        return fallback.map_err(|_| {
                            SummarizeError::LlmFailed(format!("provider error: {e}"))
                        });
                    }
                }
            }
            if summaries.is_empty() {
                return Err(SummarizeError::LlmFailed("no chunks summarized".into()));
            }
            if summaries.len() == 1 {
                Ok(summaries.into_iter().next().unwrap())
            } else {
                // Combine per-chunk summaries into one.
                Ok(summaries.join("\n\n---\n\n"))
            }
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Extract the first text output from a completion response.
fn extract_text(resp: &CompletionResponse) -> Option<String> {
    resp.output.iter().find_map(|item| match item {
        OutputItem::Text { text } => Some(text.clone()),
        _ => None,
    })
}

/// Split events into chunks by assistant turns, each chunk byte estimate
/// staying under the token budget. AssistantTextDelta is skipped (subsumed by
/// the authoritative AssistantMessage). Each chunk starts with the event that
/// begins an API round and ends just before the next one.
fn chunk_by_assistant_turn(events: &[TurnEvent], chunk_token_limit: usize) -> Vec<Vec<TurnEvent>> {
    let mut chunks: Vec<Vec<TurnEvent>> = Vec::new();
    let mut current: Vec<TurnEvent> = Vec::new();
    let mut current_bytes: usize = 0;

    for event in events {
        if matches!(event.kind, TurnEventKind::AssistantTextDelta { .. }) {
            continue;
        }
        let event_bytes = event_byte_len(event);

        // Start a new chunk when an AssistantMessage begins a new API round
        // and the current chunk is non-empty and over budget.
        if matches!(event.kind, TurnEventKind::AssistantMessage { .. })
            && !current.is_empty()
            && current_bytes > chunk_token_limit * 4
        {
            chunks.push(std::mem::take(&mut current));
            current_bytes = 0;
        }

        current.push(event.clone());
        current_bytes += event_bytes;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Rough byte length of an event text content (for chunk budgeting).
fn event_byte_len(event: &TurnEvent) -> usize {
    match &event.kind {
        TurnEventKind::UserInput { text }
        | TurnEventKind::MetaUser { text }
        | TurnEventKind::MidTurnInput { text }
        | TurnEventKind::MemoryRecall { text, .. } => text.len(),
        TurnEventKind::RewardObservation { .. } => 0,
        TurnEventKind::Unknown => 0,
        TurnEventKind::AssistantMessage { text, thinking } => {
            text.len() + thinking.as_ref().map(String::len).unwrap_or(0)
        }
        TurnEventKind::AssistantTextDelta { .. } => 0,
        TurnEventKind::ToolCall { input, .. } => input.to_string().len(),
        TurnEventKind::ToolResult { output, .. } => output.to_string().len(),
        TurnEventKind::Reasoning { text } => text.len(),
        TurnEventKind::CompactionBoundary { .. } => 0,
        TurnEventKind::CacheBreak { .. } => 0,
        TurnEventKind::Summary { text } => text.len(),
        TurnEventKind::PermissionDecision { .. } => 0,
        TurnEventKind::TurnAborted { reason } => reason.len(),
        TurnEventKind::TruncationVerdict { .. } => 0,
        TurnEventKind::WorktreeEnter { .. } | TurnEventKind::WorktreeExit { .. } => 0,
        TurnEventKind::TurnUsage { .. }
        | TurnEventKind::HookSignal { .. }
        | TurnEventKind::TurnStarted { .. }
        | TurnEventKind::SubagentSpawn { .. }
        | TurnEventKind::SubagentReturn { .. }
        | TurnEventKind::NotificationInjected { .. } => 0,
    }
}

/// Result of a compress operation. Carries the manifest, whether any events
/// were actually folded (no-progress detection), and a count of folded turns.
#[derive(Debug, Clone)]
pub struct CompressResult {
    /// The persisted manifest (also written to the backend).
    pub manifest: CheckpointManifest,
    /// Number of events folded into the summary (Summarized disposition).
    pub folded_count: usize,
    /// True when at least one event was Summarized (progress was made).
    pub made_progress: bool,
}

/// Run the Compress stage for a session: build a manifest from the current
/// event log, produce a summary for the folded span, persist the manifest via
/// write_checkpoint, and append CompactionBoundary + Summary events to the log
/// through SessionStore::append so the hash chain is maintained.
///
/// The caller (the agent loop overflow handler or pre-flight gate) checks
/// made_progress: when false, the manifest is all-Verbatim and compressing
/// again would not shrink the window — the caller must fail-closed instead of
/// looping.
///
/// The summarizer is the LlmSummarizer when a provider is wired, or
/// HeuristicSummarizer when no provider is available (tests, offline).
pub async fn commit_manifest(
    store: &dyn houyicoder_api::session::SessionLog,
    session: SessionId,
    manifest: &CheckpointManifest,
) -> Result<CompressResult, houyicoder_context::ContextError> {
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
            .append(TurnEvent {
                id: EventId::new(),
                session,
                ts: now,
                prev_hash: None,
                kind: TurnEventKind::CompactionBoundary {
                    checkpoint: manifest.id,
                },
            })
            .await?;
        store
            .append(TurnEvent {
                id: EventId::new(),
                session,
                ts: now,
                prev_hash: None,
                kind: TurnEventKind::Summary {
                    text: summary_text.clone(),
                },
            })
            .await?;
    }
    Ok(CompressResult {
        manifest: manifest.clone(),
        folded_count,
        made_progress,
    })
}

/// Run the Compress stage: build a manifest then commit it. Tests use this
/// directly; the production Runner.compress builds the manifest itself so it
/// can run before-compact marker extraction between build and commit without
/// paying the summarizer twice. custom_instructions threads PreCompact hook
/// output (or a future /compact argument) into the summarizer prompt.
pub async fn compress_session(
    store: &dyn houyicoder_api::session::SessionLog,
    session: SessionId,
    events: &[TurnEvent],
    policy: &CompressPolicy,
    summarizer: &dyn Summarizer,
    custom_instructions: Option<&str>,
) -> Result<CompressResult, houyicoder_context::ContextError> {
    let manifest = build_manifest(events, policy, summarizer, custom_instructions).await;
    commit_manifest(store, session, &manifest).await
}

/// Deterministic before-compact marker extraction (sub-5ms, no model). Scans the events the manifest marks Summarized (the
/// about-to-fold span) for unsolved-problem and key-decision markers, returning
/// memory entries to write to the auto scope so key facts survive the fold. The
/// marker recall is below 100 percent — the async LLM pass (a future step)
/// catches unphrased decisions this rule scan misses. This fast tier is the
/// immediacy backstop: it runs inline at compact time with zero model cost, so
/// a compact never folds a key fact away with nothing written. The lossless
/// session log is the ultimate safety net — the folded raw stays in the log
/// for the async tier to re-scan.
pub(crate) fn extract_precompact_markers(
    events: &[TurnEvent],
    manifest: &CheckpointManifest,
) -> Vec<MemoryEntry> {
    use std::collections::HashMap;
    let plan: HashMap<EventId, Disposition> = manifest
        .plan
        .iter()
        .flat_map(|g| g.event_ids.iter().map(|id| (*id, g.disposition)))
        .collect();
    let mut out = Vec::new();
    for ev in events {
        if !matches!(plan.get(&ev.id), Some(Disposition::Summarized)) {
            continue;
        }
        let text = match &ev.kind {
            TurnEventKind::AssistantMessage { text, .. } => text.as_str(),
            TurnEventKind::UserInput { text } => text.as_str(),
            _ => continue,
        };
        for marker in find_markers(text) {
            let key = marker_key(marker.kind, text);
            out.push(
                MemoryEntry::new(key, text.to_string(), MemorySource::Feedback)
                    .with_meta(marker.phrase.to_string(), ev.ts),
            );
        }
    }
    out
}

/// Scan every model-visible event (user + assistant) for unsolved-problem +
/// key-decision markers, for the /clear path. Unlike extract_precompact_markers
/// (which scans only the about-to-fold Summarized span at compact time), clear
/// drops the whole session, so the entire event stream is the about-to-drop
/// span. The same marker phrases + key derivation are reused so the two paths
/// agree on what counts as a marker. Dedup against the existing auto-scope
/// keys happens at the caller (a marker already saved by a prior compact is
/// not re-written).
pub(crate) fn extract_preclear_markers(events: &[TurnEvent]) -> Vec<MemoryEntry> {
    let mut out = Vec::new();
    for ev in events {
        let text = match &ev.kind {
            TurnEventKind::AssistantMessage { text, .. } => text.as_str(),
            TurnEventKind::UserInput { text } => text.as_str(),
            _ => continue,
        };
        for marker in find_markers(text) {
            let key = marker_key(marker.kind, text);
            out.push(
                MemoryEntry::new(key, text.to_string(), MemorySource::Feedback)
                    .with_meta(marker.phrase.to_string(), ev.ts),
            );
        }
    }
    out
}

struct Marker {
    kind: MarkerKind,
    phrase: &'static str,
}

#[derive(Clone, Copy)]
enum MarkerKind {
    UnsolvedProblem,
    KeyDecision,
}

/// Scan text for deterministic markers. Unsolved-problem: error / panic /
/// traceback / todo / fixme / not sure / broken, or a line ending with a
/// question mark. Key-decision: chose / decided / go with / keep doing /
/// exactly / perfect. Substring match, case-insensitive — the phrases are
/// specific enough that word-boundary matching is not worth the cost at this
/// tier (the async LLM pass refines). The bare word yes is deliberately
/// omitted (it matches yesterday etc.).
///
/// One marker per kind per text: the key is kind plus a content slug, so a
/// second matching phrase of the same kind would collide on the same key +
/// the caller's add overwrites (last-phrase-wins, losing the first phrase's
/// meta). Emitting only the first matching phrase per kind avoids that —
/// the first hit is the signal, the content body is what matters.
fn find_markers(text: &str) -> Vec<Marker> {
    let lower = text.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut unsolved = false;
    for phrase in UNSOLVED_PHRASES {
        if !unsolved && lower.contains(phrase) {
            out.push(Marker {
                kind: MarkerKind::UnsolvedProblem,
                phrase,
            });
            unsolved = true;
        }
    }
    if !unsolved && lower.lines().any(|l| l.trim_end().ends_with('?')) {
        out.push(Marker {
            kind: MarkerKind::UnsolvedProblem,
            phrase: "?",
        });
    }
    let mut decision = false;
    for phrase in DECISION_PHRASES {
        if !decision && lower.contains(phrase) {
            out.push(Marker {
                kind: MarkerKind::KeyDecision,
                phrase,
            });
            decision = true;
        }
    }
    out
}

const UNSOLVED_PHRASES: &[&str] = &[
    "error",
    "panic",
    "traceback",
    "todo",
    "fixme",
    "not sure",
    "broken",
];

const DECISION_PHRASES: &[&str] = &[
    "chose",
    "decided",
    "go with",
    "keep doing",
    "exactly",
    "perfect",
];

/// Build a stable, filesystem-safe memory key for a marker hit: the marker
/// kind prefix plus a sanitized truncation of the source text. Same content
/// plus kind yields the same key, so dedup is natural (the caller skips keys
/// already in the store). The key is lowercase, alphanumeric
/// and dash only, no traversal, so it passes the memory key sanitizer.
fn marker_key(kind: MarkerKind, text: &str) -> String {
    let prefix = match kind {
        MarkerKind::UnsolvedProblem => "compact-unsolved",
        MarkerKind::KeyDecision => "compact-decision",
    };
    let slug: String = text
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(32)
        .collect();
    format!("{prefix}-{slug}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::test_support::FakeProvider;
    use houyicoder_context::{EventId, SessionId, TurnEvent, TurnEventKind};
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_protocol::llm::Usage;
    use houyicoder_protocol::llm::{CompletionResponse, OutputItem, ProviderError};
    use houyicoder_session::SessionStore;

    fn ev(session: SessionId, id: EventId, kind: TurnEventKind) -> TurnEvent {
        TurnEvent {
            id,
            session,
            ts: 0,
            prev_hash: None,
            kind,
        }
    }

    /// An Unknown kind (a future-binary event type the current binary does
    /// not recognize) carries no byte length — it budgets as zero so the
    /// chunk estimator does not choke on a forward-compatible event.
    #[test]
    fn test_byte_len_unknown_zero() {
        let e = ev(SessionId::new(), EventId::new(), TurnEventKind::Unknown);
        assert_eq!(event_byte_len(&e), 0);
    }

    /// The summarizer instruction directs a four-section summary so a resumed
    /// run can find the active task, pending steps, and key file paths rather
    /// than a free-form recap. Pin the structure against drift.
    #[test]
    fn test_summarizer_instruction_has_sections() {
        assert!(
            SUMMARIZER_INSTRUCTION.contains("Primary intent"),
            "{SUMMARIZER_INSTRUCTION}"
        );
        assert!(
            SUMMARIZER_INSTRUCTION.contains("Current work"),
            "{SUMMARIZER_INSTRUCTION}"
        );
        assert!(
            SUMMARIZER_INSTRUCTION.contains("Pending"),
            "{SUMMARIZER_INSTRUCTION}"
        );
        assert!(
            SUMMARIZER_INSTRUCTION.contains("Key files"),
            "{SUMMARIZER_INSTRUCTION}"
        );
        assert!(
            SUMMARIZER_INSTRUCTION.contains("plain-text"),
            "{SUMMARIZER_INSTRUCTION}"
        );
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

    #[tokio::test]
    async fn test_llm_summarizer_produces_summary() {
        // A stub provider that returns a canned summary. The LlmSummarizer
        // calls complete() and extracts the text.
        let resp = CompletionResponse {
            output: vec![OutputItem::Text {
                text: "This is a summary of the conversation.".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        };
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![resp]));
        let summarizer = LlmSummarizer::new(provider, "test".into());
        let s = SessionId::new();
        let events = vec![
            ev(s, EventId::new(), user("do the task")),
            ev(s, EventId::new(), assistant("working on it")),
            ev(s, EventId::new(), assistant("done")),
        ];
        let summary = summarizer.summarize(&events, None).await.unwrap();
        assert!(summary.contains("summary"));
    }

    #[tokio::test]
    async fn test_llm_summarizer_fallback_error() {
        // A provider that always returns an error. The summarizer falls back
        // to the heuristic, which returns a non-empty placeholder.
        struct ErrorProvider;
        impl ModelProvider for ErrorProvider {
            fn complete(
                &self,
                _req: CompletionRequest,
            ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
                Box::pin(async move { Err(ProviderError::Unknown("no llm".into())) })
            }
            fn stream(
                &self,
                _req: CompletionRequest,
            ) -> houyicoder_async::PStream<
                '_,
                Result<houyicoder_protocol::llm::LlmEvent, ProviderError>,
            > {
                Box::pin(futures::stream::iter(Vec::new()))
            }
            fn capabilities(&self) -> houyicoder_protocol::llm::ModelCapabilities {
                houyicoder_protocol::llm::ModelCapabilities::default()
            }
        }
        let provider: Arc<dyn ModelProvider> = Arc::new(ErrorProvider);
        let summarizer = LlmSummarizer::new(provider, "test".into());
        let s = SessionId::new();
        let events = vec![
            ev(s, EventId::new(), user("hi")),
            ev(s, EventId::new(), assistant("hello")),
        ];
        let summary = summarizer.summarize(&events, None).await.unwrap();
        assert!(!summary.is_empty(), "heuristic fallback must produce text");
    }

    #[tokio::test]
    async fn test_llm_summarizer_empty_events() {
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("summary"));
        let summarizer = LlmSummarizer::new(provider, "test".into());
        let result = summarizer.summarize(&[], None).await;
        assert!(matches!(result, Err(SummarizeError::Empty)));
    }

    /// Custom instructions merge into the summarizer prompt (the PreCompact
    /// return channel). The LlmSummarizer formats the base instruction + the
    /// extra; the provider receives the merged string. A canned-summary stub
    /// provider returns text so the merge branch executes + the summary lands.
    #[tokio::test]
    async fn test_llm_summarizer_merges_instructions() {
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("merged summary"));
        let summarizer = LlmSummarizer::new(provider, "test".into());
        let s = SessionId::new();
        let events = vec![
            ev(s, EventId::new(), user("do the task")),
            ev(s, EventId::new(), assistant("working on it")),
        ];
        let summary = summarizer
            .summarize(&events, Some("focus on the API design"))
            .await
            .unwrap();
        assert!(summary.contains("merged summary"));
    }

    /// Empty custom instructions fall to the default instruction branch (the
    /// Some-empty guard rejects an empty string, taking the wildcard arm).
    /// Pins that an empty string is treated as no instructions, not a
    /// trailing separator.
    #[tokio::test]
    async fn test_llm_summarizer_empty_instructions() {
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("summary"));
        let summarizer = LlmSummarizer::new(provider, "test".into());
        let s = SessionId::new();
        let events = vec![
            ev(s, EventId::new(), user("hi")),
            ev(s, EventId::new(), assistant("hello")),
        ];
        let summary = summarizer.summarize(&events, Some("")).await.unwrap();
        assert!(summary.contains("summary"));
    }

    /// A provider that returns a response with no text output triggers the
    /// heuristic fallback (the no-text-in-response arm). The fallback returns
    /// a non-empty placeholder so the pipeline never bricks.
    #[tokio::test]
    async fn test_llm_fallback_no_text() {
        let resp = CompletionResponse {
            output: vec![],
            usage: Usage::default(),
            model: "test".into(),
        };
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![resp]));
        let summarizer = LlmSummarizer::new(provider, "test".into());
        let s = SessionId::new();
        let events = vec![
            ev(s, EventId::new(), user("hi")),
            ev(s, EventId::new(), assistant("hello")),
        ];
        let summary = summarizer.summarize(&events, None).await.unwrap();
        assert!(!summary.is_empty(), "heuristic fallback on no-text");
    }

    #[tokio::test]
    async fn test_compress_writes_checkpoint_events() {
        // compress_session produces a manifest, persists it via
        // write_checkpoint, and appends CompactionBoundary + Summary events
        // through SessionStore::append so the hash chain is maintained.
        let store = std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
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
        let result = compress_session(&*store, s, &events, &policy, &HeuristicSummarizer, None)
            .await
            .unwrap();
        assert!(result.made_progress, "must fold some events");
        assert!(result.folded_count > 0);
        // Manifest persisted: read_checkpoint round-trips.
        let back = store.read_checkpoint(result.manifest.id).await.unwrap();
        assert_eq!(back.summary, result.manifest.summary);
        assert_eq!(back.plan.len(), result.manifest.plan.len());
        // CompactionBoundary + Summary appended to the log.
        let replay = store.replay(s).await.unwrap();
        let boundary_count = replay
            .iter()
            .filter(|e| matches!(e.kind, TurnEventKind::CompactionBoundary { .. }))
            .count();
        assert_eq!(boundary_count, 1, "one compaction boundary");
        let summary_count = replay
            .iter()
            .filter(|e| matches!(e.kind, TurnEventKind::Summary { .. }))
            .count();
        assert_eq!(summary_count, 1, "one summary event");
    }

    #[tokio::test]
    async fn test_no_progress_keeps_all() {
        // When all events are Verbatim (fewer turns than tail_turns), compress
        // makes no progress. The caller must detect this and fail-closed.
        let store = std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let s = SessionId::new();
        let ids = ids(2);
        let events = vec![ev(s, ids[0], user("task")), ev(s, ids[1], assistant("a1"))];
        let policy = CompressPolicy {
            tail_turns: 4,
            preserve_recent_tokens: 0,
            large_output_bytes: 0,
        };
        let result = compress_session(&*store, s, &events, &policy, &HeuristicSummarizer, None)
            .await
            .unwrap();
        assert!(!result.made_progress, "all verbatim = no progress");
        assert_eq!(result.folded_count, 0);
        assert!(result.manifest.summary.is_none());
    }

    #[tokio::test]
    async fn test_compress_empty_events() {
        // Empty event log: no manifest plan, no summary, no boundary.
        let store = std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let s = SessionId::new();
        let policy = CompressPolicy::default();
        let result = compress_session(&*store, s, &[], &policy, &HeuristicSummarizer, None)
            .await
            .unwrap();
        assert!(!result.made_progress);
        assert!(result.manifest.plan.is_empty());
        assert!(result.manifest.summary.is_none());
        let replay = store.replay(s).await.unwrap();
        assert!(replay.is_empty(), "no boundary/summary for empty events");
    }

    #[tokio::test]
    async fn test_compress_checkpoint_round_trips() {
        // The persisted manifest survives a read_checkpoint round-trip: the
        // plan, summary, and id all match.
        let store = std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
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
        let result = compress_session(&*store, s, &events, &policy, &HeuristicSummarizer, None)
            .await
            .unwrap();
        let id = result.manifest.id;
        let back = store.read_checkpoint(id).await.unwrap();
        assert_eq!(back.id, id);
        assert_eq!(back.session, s);
        assert_eq!(back.summary, result.manifest.summary);
        assert_eq!(back.plan, result.manifest.plan);
        // list_checkpoints returns it.
        let list = store.list_checkpoints(s).await.unwrap();
        assert!(list.contains(&id));
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod lifecycle_tests;
