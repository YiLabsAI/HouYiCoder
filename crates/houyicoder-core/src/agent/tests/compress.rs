use super::*;
use crate::provider::test_support::FakeProvider;
use houyicoder_context::TurnEventKind;
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{CompletionResponse, ModelCapabilities, OutputItem, ProviderError};
use houyicoder_protocol::llm::{LlmEvent, Usage};
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;

// runner_with is shared from the parent tests module; reuse rather than duplicate.
use super::runner_with;

/// The runner's active model seeds from config.model and swaps on set_model
/// (the /model pane select). The provider is stateless about the model, so the
/// swap is a cheap id change, not a provider rebuild.
#[test]
fn test_set_model_swaps_id() {
    let runner = runner_with(
        Arc::new(SmallWindowProvider::new("done", 200)),
        ToolRegistry::new(),
    );
    assert_eq!(runner.active_model(), "test", "seeds from config.model");
    runner.set_model("glm-5.2".to_string());
    assert_eq!(runner.active_model(), "glm-5.2", "set_model swaps the id");
}

fn runner_with_cfg0() -> RunnerConfig {
    RunnerConfig {
        model: "test".into(),
        instructions: "you are a test agent".into(),
        max_turns: 5,
        max_output_tokens: 8_000,
        retry: Retry {
            max_attempts: 2,
            ..Retry::default()
        },
    }
}

// ===== Compress E2E: compress then checkpoint then loop applies =====

#[tokio::test]
async fn test_compress_writes_checkpoint() {
    // Compress a session with enough events to fold, then verify the next
    // current_view returns a manifest and the served view is smaller.
    let store = std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    // Append enough assistant turns that compress has something to fold.
    for i in 0..6 {
        store
            .append(houyicoder_context::TurnEvent {
                id: houyicoder_context::EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                kind: if i == 0 {
                    TurnEventKind::UserInput {
                        text: "do the work".into(),
                    }
                } else {
                    TurnEventKind::AssistantMessage {
                        text: format!("response {i}"),
                        thinking: None,
                    }
                },
            })
            .await
            .unwrap();
    }
    let runner = Runner::new(
        store,
        Arc::new(FakeProvider::text("done")),
        ToolRegistry::new(),
        runner_with_cfg0(),
    );
    let progress = runner.compress(session).await.unwrap();
    assert!(progress, "compress must fold events");
    // current_view now returns a manifest — the checkpoint was persisted.
    let snap = runner.store().current_view(session).await.unwrap();
    assert!(snap.manifest.is_some(), "manifest must be loaded");
    assert!(snap.last_checkpoint.is_some(), "checkpoint id present");
    assert!(!snap.rewind_points.is_empty(), "rewind points exist");
    // The log now has CompactionBoundary + Summary events.
    let events = runner.store().replay(session).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, TurnEventKind::CompactionBoundary { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, TurnEventKind::Summary { .. }))
    );
}

// ===== Pre-flight trips compress =====

/// A provider with a small context window so pre-flight triggers on a modest
/// conversation. Returns a canned text response on success.
struct SmallWindowProvider {
    response: CompletionResponse,
    caps: ModelCapabilities,
}

impl SmallWindowProvider {
    fn new(text: &str, context_window: u32) -> Self {
        Self {
            response: CompletionResponse {
                output: vec![OutputItem::Text {
                    text: text.to_string(),
                }],
                usage: Usage::default(),
                model: "test".into(),
            },
            caps: ModelCapabilities {
                streaming: true,
                tools: false,
                vision: false,
                context_window,
                max_output_tokens: 8_000,
            },
        }
    }
}

impl ModelProvider for SmallWindowProvider {
    fn complete(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> houyicoder_async::PFut<'_, Result<CompletionResponse, ProviderError>> {
        let resp = self.response.clone();
        Box::pin(async move { Ok(resp) })
    }
    fn stream(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
        houyicoder_api::provider::stream_from_response(self.response.clone())
    }
    fn capabilities(&self) -> ModelCapabilities {
        self.caps
    }
}

#[tokio::test]
async fn test_pre_flight_trips_compress() {
    // Window 22000 with max_output 8000: pre_flight_threshold = 22000 - 21k =
    // 1000. The served view (system prompt + a few events ≈ 2-3k) lands in
    // (threshold, window/2] = (1000, 11000] — so the economy gate (served >
    // window/2) SKIPS and ONLY the pre-flight path (served > threshold) can
    // fire. This is path attribution: a mutation that flips the pre-flight
    // comparison (>) to (<) now removes the compact + checkpoint, failing
    // the test — the prior window=200 made economy fire first (served >
    // window/2=100) so the pre-flight mutation survived (false-green, #113).
    // After compress (folding the older events), the next iteration has a
    // manifest applied and the served view is smaller.
    let p = Arc::new(SmallWindowProvider::new("done", 22000));
    let runner = Runner::new(
        std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        p,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry {
                max_attempts: 1,
                ..Retry::default()
            },
        },
    );
    let session = SessionId::new();
    // Pre-populate with enough events that compress has something to fold.
    for i in 0..6 {
        runner
            .store()
            .append(houyicoder_context::TurnEvent {
                id: houyicoder_context::EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                kind: if i == 0 {
                    TurnEventKind::UserInput {
                        text: "do the work".into(),
                    }
                } else {
                    TurnEventKind::AssistantMessage {
                        text: format!("response {i}"),
                        thinking: None,
                    }
                },
            })
            .await
            .unwrap();
    }
    let result = runner.run(session, "continue".into()).await;
    // The run should either succeed (pre-flight compressed enough) or
    // fail-closed with an overflow error (if the system prompt alone is too
    // big for 200-token window even after compress). Either way, it must NOT
    // be ProviderFatal(ContextOverflow) — the pre-flight prevents sending.
    match result {
        Ok(r) => {
            assert!(matches!(
                r.outcome,
                RunOutcome::FinalOutput(_)
                    | RunOutcome::Interrupted(_)
                    | RunOutcome::MaxTurnsReached { .. }
            ));
        }
        Err(e) => {
            assert!(
                matches!(
                    e,
                    RunError::ContextOverflowBounded { .. } | RunError::ContextOverflowNoProgress
                ),
                "expected overflow error, got {e:?}"
            );
        }
    }
    // Prove compress actually ran: the overflow-retry path calls
    // compact_internal, which commits a checkpoint via write_checkpoint.
    // A tautology here would let a regression that drops compress pass.
    let snap = runner.store().current_view(session).await.unwrap();
    assert!(
        snap.last_checkpoint.is_some(),
        "compress must persist a checkpoint the run can rewind to"
    );
}

// ===== Pre-flight compress re-injects memory recall =====

/// A pre-flight compress that makes progress must re-inject memory recall
/// so the model is not memory-blind for the rest of the run: compact folds
/// older memory-recall events out of the served view (Summarized), and the
/// re-inject surfaces them again. This pins that the re-inject call on the
/// hot path actually executes when pre-flight trips — without it, the line
/// is dead (the prior weak pre-flight test never exceeded the window).
#[tokio::test]
async fn test_compress_runs_reinject() {
    // Tiny window plus long pre-populated turns so the served view exceeds
    // 95% of the window on the first model call. Eight 200-char assistant
    // turns far exceed 200 tokens regardless of the tokenizer ratio.
    let p = Arc::new(SmallWindowProvider::new("done", 200));
    let runner = Runner::new(
        std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        p,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry {
                max_attempts: 1,
                ..Retry::default()
            },
        },
    );
    let session = SessionId::new();
    for i in 0..8 {
        runner
            .store()
            .append(houyicoder_context::TurnEvent {
                id: houyicoder_context::EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                kind: if i == 0 {
                    TurnEventKind::UserInput {
                        text: "do the work".into(),
                    }
                } else {
                    TurnEventKind::AssistantMessage {
                        text: "x".repeat(200),
                        thinking: None,
                    }
                },
            })
            .await
            .unwrap();
    }
    // The run may succeed (compress freed enough room) or fail-closed
    // (overflow bounded or no-progress if the verbatim tail alone still
    // exceeds the tiny window). Either way, pre-flight must have fired
    // compress at least once, proven by a CompactionBoundary in the log —
    // which only lands when compress made progress, the path the re-inject
    // runs on.
    drop(runner.run(session, "continue the work".into()).await);
    let events = runner.store().replay(session).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, TurnEventKind::CompactionBoundary { .. })),
        "pre-flight must trip compress when the served view exceeds the window"
    );
}

// ===== Overflow handler: compress then retry =====

// Overflow-path tests (OverflowThenSucceedProvider, OverflowProvider, the
// bounded/no-progress/retries/abortable cases) live in tests_overflow.rs:
// split for the file-size gate, and routed to a unique model id so their
// enforced-limit writes do not leak into the "test" window other tests here
// resolve.

/// A stream that never sends a chunk (dead socket / half-open gateway) must
/// trip the idle watchdog, not hang forever. The test cfg makes the timeout
/// 50ms so this runs instantly; the bounded retry chain exhausts and the run
/// fails with Network.
#[tokio::test]
async fn test_stream_stall_aborts() {
    use houyicoder_protocol::llm::ProviderError;
    let p = Arc::new(super::tests::HangingProvider::new(vec![]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap_err();
    assert!(
        matches!(result, RunError::ProviderFatal(ProviderError::Network)),
        "stall must fail with Network: {result:?}"
    );
}

/// A stall mid-stream (one delta then pending) flushes the partial text
/// before failing, so the turn's partial is not lost.
#[tokio::test]
async fn test_stall_flushes_partial() {
    use houyicoder_protocol::llm::{LlmEvent, ProviderError};
    let p = Arc::new(super::tests::HangingProvider::new(vec![
        LlmEvent::TextDelta {
            id: "t1".into(),
            text: "partial".into(),
        },
    ]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap_err();
    assert!(
        matches!(result, RunError::ProviderFatal(ProviderError::Network)),
        "mid-stream stall must fail with Network: {result:?}"
    );
    let events = runner.store().replay(session).await.expect("replay");
    assert!(
        events.iter().any(|e| matches!(
            e.kind,
            houyicoder_context::TurnEventKind::AssistantMessage { .. }
        )),
        "partial text flushed before the stall failure"
    );
}

/// Runner.compress runs before-compact marker extraction when a memory
/// provider is wired: the about-to-fold span is scanned for markers + hits
/// are written (best-effort). With a stub provider (add = no-op) this covers
/// the wiring path; the marker extraction logic itself is tested in
/// lifecycle.rs (the pure fn).
#[tokio::test]
async fn test_compress_runs_marker_extraction() {
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let memory: Arc<dyn houyicoder_api::memory::MemoryProvider> =
        Arc::new(houyicoder_memory::StubMemoryProvider::new());
    let runner = runner_with(provider, ToolRegistry::new()).with_memory(memory);
    let session = houyicoder_context::SessionId::new();
    // Seed 6 assistant turns so default tail_turns=4 folds the oldest 2.
    // Their text contains an unsolved marker so extract_precompact_markers
    // finds hits; the stub add accepts them (no-op).
    for i in 0..6 {
        runner
            .store()
            .append(houyicoder_context::TurnEvent {
                id: houyicoder_context::EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                kind: TurnEventKind::AssistantMessage {
                    text: format!("turn {i} hit an error"),
                    thinking: None,
                },
            })
            .await
            .unwrap();
    }
    let progress = runner.compress(session).await;
    assert!(progress.is_ok(), "compress must not error");
    assert!(
        progress.unwrap(),
        "must make progress (fold the oldest turns)"
    );
}

/// A recording memory for the before_clear test: records every add + lists
/// the pre-seeded keys so dedup is exercised.
struct RecordingClearMemory {
    written: std::sync::Mutex<Vec<houyicoder_context::MemoryEntry>>,
    existing_keys: std::sync::Mutex<Vec<String>>,
}
impl houyicoder_api::memory::MemoryProvider for RecordingClearMemory {
    fn recall(
        &self,
        _q: &str,
        _b: usize,
        _surfaced: &std::collections::HashSet<String>,
    ) -> Vec<houyicoder_context::MemoryEntry> {
        Vec::new()
    }
    fn add(
        &self,
        e: houyicoder_context::MemoryEntry,
    ) -> Result<(), houyicoder_context::MemoryError> {
        self.written.lock().expect("written").push(e);
        Ok(())
    }
    fn list_memories(&self) -> Vec<houyicoder_context::MemorySummary> {
        self.existing_keys
            .lock()
            .expect("existing")
            .iter()
            .map(|k| {
                houyicoder_context::MemorySummary::new(
                    k.clone(),
                    "pre-seeded",
                    houyicoder_context::MemorySource::Feedback,
                    houyicoder_context::MemoryScope::Auto,
                    0,
                )
            })
            .collect()
    }
}

/// Runner.before_clear scans the whole session for markers + writes them to
/// the auto scope, so key facts survive the /clear drop (the do-not-lose
/// invariant). Both an unsolved + a decision marker are written.
#[tokio::test]
async fn test_before_clear_writes_markers() {
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let memory: Arc<RecordingClearMemory> = Arc::new(RecordingClearMemory {
        written: std::sync::Mutex::new(Vec::new()),
        existing_keys: std::sync::Mutex::new(Vec::new()),
    });
    let runner = runner_with(provider, ToolRegistry::new())
        .with_memory(Arc::clone(&memory) as Arc<dyn houyicoder_api::memory::MemoryProvider>);
    let session = houyicoder_context::SessionId::new();
    for text in ["hit an error here", "we decided to use rust", "plain turn"] {
        runner
            .store()
            .append(houyicoder_context::TurnEvent {
                id: houyicoder_context::EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                kind: TurnEventKind::AssistantMessage {
                    text: text.into(),
                    thinking: None,
                },
            })
            .await
            .unwrap();
    }
    runner.before_clear(session).await.expect("before_clear ok");
    let written = memory.written.lock().expect("written").clone();
    assert!(
        written
            .iter()
            .any(|e| e.key.starts_with("compact-unsolved")),
        "unsolved marker written"
    );
    assert!(
        written
            .iter()
            .any(|e| e.key.starts_with("compact-decision")),
        "decision marker written"
    );
    assert!(
        written.iter().all(|e| e.content != "plain turn"),
        "non-marker text not written"
    );
}

/// before_clear dedups against existing auto-scope keys: a marker already
/// saved (by a prior compact or a prior clear) is not re-written.
#[tokio::test]
async fn test_before_clear_dedups_existing() {
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let memory: Arc<RecordingClearMemory> = Arc::new(RecordingClearMemory {
        written: std::sync::Mutex::new(Vec::new()),
        existing_keys: std::sync::Mutex::new(vec!["compact-unsolved-error".into()]),
    });
    let runner = runner_with(provider, ToolRegistry::new())
        .with_memory(Arc::clone(&memory) as Arc<dyn houyicoder_api::memory::MemoryProvider>);
    let session = houyicoder_context::SessionId::new();
    runner
        .store()
        .append(houyicoder_context::TurnEvent {
            id: houyicoder_context::EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: "hit an error here".into(),
                thinking: None,
            },
        })
        .await
        .unwrap();
    runner.before_clear(session).await.expect("before_clear ok");
    let written = memory.written.lock().expect("written").clone();
    assert!(
        written.iter().all(|e| e.key != "compact-unsolved-error"),
        "the pre-existing unsolved marker key is not re-written"
    );
}

/// before_clear is a no-op (no panic, no write) when no memory provider is
/// wired — the clear still proceeds on a memory-less runner.
#[tokio::test]
async fn test_before_noop_without_memory() {
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let runner = runner_with(provider, ToolRegistry::new());
    let session = houyicoder_context::SessionId::new();
    runner.before_clear(session).await.expect("no-op ok");
}

// ===== Max turns: graceful Ok result (not Err crash) =====

/// Hitting the turn cap is a graceful Ok outcome carrying turns + usage,
/// not a thrown error.
#[tokio::test]
async fn test_run_max_turns_reached() {
    // Always returns a tool call → never final → graceful MaxTurnsReached.
    let resp = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "echo".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let p = Arc::new(FakeProvider::new(vec![resp]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(StubTool::new("echo")));
    let runner = runner_with(p, tools);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.unwrap();
    assert!(matches!(
        result.outcome,
        RunOutcome::MaxTurnsReached { turns } if turns == 5
    ));
    assert_eq!(result.turns, 5);
    // The provider omits usage (Usage::default()); the served-token
    // fallback fills input_tokens so the status gauge + tally read the real
    // footprint, not a silent 0.
    assert!(
        result.usage.input_tokens > 0,
        "omitted usage falls back to the served count: {:#?}",
        result.usage
    );
}
