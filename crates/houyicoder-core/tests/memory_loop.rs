//! Real E2E for the memory closed loop: recall injects a durable
//! memory-recall attachment into the served message stream (merged into the
//! user turn, not the system prompt, so the system prompt stays byte-frozen
//! for prompt-cache across turns), write persists facts from /save signals,
//! and compaction folds old memory-recall events out of the projection so
//! the surfaced de-dup set naturally empties and entries re-surface.
//!
//! Uses a custom MemoryProvider that records calls and honors the surfaced
//! param so the de-dup lifecycle is verifiable without a filesystem.

use std::sync::{Arc, Mutex};

use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::provider::stream_from_response;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{MemoryEntry, MemorySource, SessionId};
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, InputItem, ModelCapabilities, OutputItem, ProviderError,
};
use houyicoder_protocol::llm::{LlmEvent, Usage};
use houyicoder_session::SessionStore;
use std::collections::HashSet;

use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{RunOutcome, Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;

/// A memory provider that returns scripted entries on recall (skipping any
/// key in surfaced, the caller-built de-dup set) and records every call so
/// the surfaced-state lifecycle is verifiable without a filesystem.
struct RecordingMemory {
    entries: Mutex<Vec<MemoryEntry>>,
    recall_count: Mutex<usize>,
    written: Mutex<Vec<MemoryEntry>>,
}

impl RecordingMemory {
    fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            recall_count: Mutex::new(0),
            written: Mutex::new(Vec::new()),
        }
    }

    fn seed(&self, entry: MemoryEntry) {
        self.entries.lock().expect("entries").push(entry);
    }

    fn recall_count(&self) -> usize {
        *self.recall_count.lock().expect("recall count")
    }

    fn written_entries(&self) -> Vec<MemoryEntry> {
        self.written.lock().expect("written").clone()
    }
}

impl MemoryProvider for RecordingMemory {
    fn recall(&self, _query: &str, _budget: usize, surfaced: &HashSet<String>) -> Vec<MemoryEntry> {
        *self.recall_count.lock().expect("recall count") += 1;
        self.entries
            .lock()
            .expect("entries")
            .iter()
            .filter(|e| !surfaced.contains(&e.key))
            .cloned()
            .collect()
    }

    fn add(&self, entry: MemoryEntry) -> Result<(), houyicoder_context::MemoryError> {
        self.written.lock().expect("written").push(entry);
        Ok(())
    }
}

/// One captured request: the instructions string plus the concatenated
/// user-message text the model was served. The user text carries the
/// memory-recall attachment merged into the turn's user input.
#[derive(Clone)]
struct CapturedRequest {
    instructions: String,
    user_text: String,
}

impl CapturedRequest {
    fn from(req: &CompletionRequest) -> Self {
        let user_text = req
            .input
            .iter()
            .filter_map(|item| match item {
                InputItem::User { content } => Some(content.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            instructions: req.instructions.clone(),
            user_text,
        }
    }
}

/// A provider that records each request before returning a canned final
/// response.
struct FinalResponseProvider {
    seen: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl FinalResponseProvider {
    fn record(&self, req: &CompletionRequest) {
        self.seen
            .lock()
            .expect("seen")
            .push(CapturedRequest::from(req));
    }
}

impl ModelProvider for FinalResponseProvider {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.record(&req);
        let resp = CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".to_string(),
        };
        Box::pin(async move { Ok(resp) })
    }

    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        self.record(&req);
        stream_from_response(CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".to_string(),
        })
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// When static config.instructions are appended to the served system prompt,
/// the recalled memory still reaches the model via the message stream — it is
/// a tail attachment merged into the user turn, not part of the system prompt.
/// This is the regression the durable-tail pivot prevents: memory used to live
/// in system_text, so a config.instructions replace silently dropped it.
#[tokio::test]
async fn test_instructions_override_keeps_memory() {
    let memory = Arc::new(RecordingMemory::new());
    memory.seed(MemoryEntry::new(
        "rust-conventions",
        "Prefer let chains in conditionals",
        MemorySource::Project,
    ));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(FinalResponseProvider { seen: seen.clone() });
    let runner = Runner::new(
        std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            instructions: "custom static instructions".to_string(),
            ..RunnerConfig::default()
        },
    )
    .with_memory(memory.clone());

    let session = SessionId::new();
    let result = runner
        .run(session, "help with rust conventions".into())
        .await;
    assert!(result.is_ok(), "run must succeed");
    let captured = seen.lock().expect("seen").clone();
    // The configured instructions are APPENDED to the served system prompt
    // (not a replace) so the byte-stable prefix survives for prompt-cache.
    // Both the assembled identity and the configured text are present.
    assert!(
        captured[0]
            .instructions
            .contains("You are houyicoder, an AI coding assistant"),
        "assembled system prompt kept (not replaced)"
    );
    assert!(
        captured[0]
            .instructions
            .contains("custom static instructions"),
        "configured instructions appended"
    );
    // Memory still reaches the model via the user-message text, not the
    // system prompt — the tail attachment is immune to it.
    assert!(
        captured[0].user_text.contains("Prefer let chains"),
        "memory must survive the instructions append via the user text: {}",
        captured[0].user_text
    );
}

fn runner_with_memory(provider: Arc<dyn ModelProvider>, memory: Arc<dyn MemoryProvider>) -> Runner {
    Runner::new(
        std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    )
    .with_memory(memory)
}

/// The recalled memory entry appears in the user-message text served to the
/// provider, NOT in the instructions (system prompt). This is the core
/// closed-loop assertion: memory is a durable tail attachment merged into the
/// turn's user input, so the system prompt stays byte-stable across turns
/// (prompt-cache friendly) — memory no longer lands in the system prompt.
#[tokio::test]
async fn test_recall_injects_user_text() {
    let memory = Arc::new(RecordingMemory::new());
    memory.seed(MemoryEntry::new(
        "rust-conventions",
        "Prefer let chains in conditionals",
        MemorySource::Project,
    ));

    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(FinalResponseProvider { seen: seen.clone() });
    let runner = runner_with_memory(provider, memory.clone());

    let session = SessionId::new();
    let result = runner
        .run(session, "help with rust conventions".into())
        .await;
    assert!(result.is_ok(), "run must succeed");
    assert!(matches!(result.unwrap().outcome, RunOutcome::FinalOutput(t) if t == "done"));

    let captured = seen.lock().expect("seen").clone();
    assert!(
        !captured.is_empty(),
        "the loop must have called the provider"
    );
    // Memory lands in the user-message text (merged attachment), not the
    // system prompt / instructions.
    assert!(
        captured[0].user_text.contains("Prefer let chains"),
        "recalled memory must appear in the served user text: {}",
        captured[0].user_text
    );
    assert!(
        captured[0].user_text.contains("rust-conventions"),
        "memory key must appear in the served user text"
    );
    assert!(
        !captured[0].instructions.contains("Prefer let chains"),
        "memory must NOT be in the system prompt (byte-frozen): {}",
        captured[0].instructions
    );
    assert!(memory.recall_count() > 0, "recall must have been called");
}

/// A /save signal in the user input is extracted and written via add after
/// the run completes. This is the deterministic fact-extraction path — no
/// model classifier on the hot path.
#[tokio::test]
async fn test_save_signal_writes_memory() {
    let memory = Arc::new(RecordingMemory::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(FinalResponseProvider { seen: seen.clone() });
    let runner = runner_with_memory(provider, memory.clone());

    let session = SessionId::new();
    let input = "/save test-fact user: Always verify before committing";
    let result = runner.run(session, input.into()).await;
    assert!(result.is_ok());

    let written = memory.written_entries();
    assert_eq!(written.len(), 1, "one fact must be written");
    assert_eq!(written[0].key, "test-fact");
    assert_eq!(written[0].source, MemorySource::User);
    assert!(written[0].content.contains("Always verify"));
}

/// Compress does not call recall — recall fires only at the run-entry turn
/// boundary (per user query), never on the compaction path. The surfaced
/// reset is now a projection-level effect (old memory-recall events fold
/// out), not a provider-side clear, so compress must not touch recall.
#[tokio::test]
async fn test_compress_does_not_recall() {
    let memory = Arc::new(RecordingMemory::new());
    memory.seed(MemoryEntry::new(
        "persisted-fact",
        "Use deterministic extraction not model classifiers",
        MemorySource::Feedback,
    ));

    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(FinalResponseProvider { seen: seen.clone() });
    let runner = runner_with_memory(provider, memory.clone());

    let session = SessionId::new();
    let _result = runner.run(session, "tell me about extraction".into()).await;
    let recall_after_run = memory.recall_count();
    assert!(recall_after_run > 0, "recall must fire during run");

    let progress = runner.compress(session).await;
    assert!(progress.is_ok(), "compress must not error");
    assert_eq!(
        memory.recall_count(),
        recall_after_run,
        "compress must not call recall"
    );
}

/// After compaction folds the oldest turn (its memory-recall attachment with
/// it) out of the projection, the surfaced scan empties and the next run
/// re-surfaces the entry. This is the full closed loop: recall → inject →
/// compress (folds memory-recall) → recall again. Needs enough assistant
/// turns that the default compress policy actually folds the oldest turn.
#[tokio::test]
async fn test_post_compress_recall_surfaces() {
    let memory = Arc::new(RecordingMemory::new());
    memory.seed(MemoryEntry::new(
        "cycled-fact",
        "Memory re-surfaces after compaction boundary",
        MemorySource::Project,
    ));

    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(FinalResponseProvider { seen: seen.clone() });
    let runner = runner_with_memory(provider, memory.clone());

    let session = SessionId::new();

    // Turn 1: recall fires, memory injected into the user text.
    let _result = runner.run(session, "tell me about memory".into()).await;
    let captured = seen.lock().expect("seen").clone();
    assert!(
        captured[0].user_text.contains("cycled-fact"),
        "memory must be injected in turn 1"
    );

    // Build enough assistant turns that compress (default tail_turns=4)
    // folds the oldest turn — and its memory-recall attachment — out of the
    // projection. Turns 2..5 surface nothing new (the cycled-fact key is
    // already in the view), so only turn 1 carries a memory-recall event.
    for _ in 1..5 {
        let _r = runner.run(session, "continue".into()).await;
    }

    // Compress: folds the oldest turn incl. its memory-recall event.
    let progress = runner.compress(session).await;
    assert!(progress.is_ok(), "compress must not error");
    assert!(
        progress.unwrap(),
        "compress must make progress (fold the oldest turn)"
    );

    // Turn after compress: surfaced scan is empty (memory-recall folded),
    // so recall re-surfaces the entry.
    let _result = runner
        .run(session, "tell me about memory again".into())
        .await;
    let captured = seen.lock().expect("seen").clone();
    let latest = captured.last().expect("at least one more turn ran");
    assert!(
        latest.user_text.contains("cycled-fact"),
        "memory must re-surface post-compress in the latest turn: {}",
        latest.user_text
    );
}

/// No memory wired: the recall path is opt-in — no memory-recall attachment
/// is injected into the served user text, and no write happens. The memory
/// behavior guidance (the always-on what-to-save section in the system
/// prompt) is present by design; what is opt-in is the recall injection and
/// the write seam, both of which stay dormant with no provider. This
/// guards the recall/write opt-in seam, not the always-on guidance.
#[tokio::test]
async fn test_no_memory_no_change() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(FinalResponseProvider { seen: seen.clone() });
    let runner = Runner::new(
        std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );

    let session = SessionId::new();
    let result = runner.run(session, "normal prompt".into()).await;
    assert!(result.is_ok());
    let captured = seen.lock().expect("seen").clone();
    assert!(!captured.is_empty());
    assert!(
        !captured[0].user_text.contains("# Recalled memories"),
        "no recalled-memories attachment injected when no provider is wired"
    );
    assert!(
        !captured[0].instructions.contains("# Recalled memories"),
        "no recalled-memories in the system prompt either"
    );
}

/// A single-word query ("hi", "thanks") carries too little signal to recall
/// against, so the turn-entry gate skips recall entirely — no recall call, no
/// memory attachment. A multi-word query on a fresh session still recalls.
#[tokio::test]
async fn test_single_word_skips_recall() {
    let memory = Arc::new(RecordingMemory::new());
    memory.seed(MemoryEntry::new(
        "rust-conventions",
        "Prefer let chains in conditionals",
        MemorySource::Project,
    ));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(FinalResponseProvider { seen: seen.clone() });
    let runner = runner_with_memory(provider, memory.clone());

    let session = SessionId::new();
    let _result = runner.run(session, "hi".into()).await;
    assert_eq!(
        memory.recall_count(),
        0,
        "single-word query must skip recall"
    );
    let captured = seen.lock().expect("seen").clone();
    assert!(
        !captured[0].user_text.contains("Prefer let chains"),
        "no memory attachment for a single-word query: {}",
        captured[0].user_text
    );
    // A multi-word query on a fresh session recalls.
    let session2 = SessionId::new();
    let _result = runner.run(session2, "help with rust".into()).await;
    assert!(memory.recall_count() > 0, "multi-word query must recall");
}
