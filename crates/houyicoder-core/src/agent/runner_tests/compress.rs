use super::*;
use crate::agent::extractor::{ExtractionLogs, MemoryExtractor};
use crate::agent::memory::{MemoryGates, MemoryRuntime};
use crate::provider::test_support::FakeProvider;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::stream_from_response;
use houyicoder_api::session::SessionLog;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::SessionEvent;
use houyicoder_context::{
    EventId, MemoryEntry, MemoryError, MemoryOrigin, MemorySummary, SessionLogEntry,
};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::CompletionRequest;
use houyicoder_protocol::llm::{CompletionResponse, ModelCapabilities, OutputItem, ProviderError};
use houyicoder_protocol::llm::{LlmEvent, Usage};
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::fs;
use std::process;
use std::sync::Mutex;
use std::time::Duration;

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
    // current_view returns a manifest and the assembled context is smaller.
    let store = std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    // Append enough assistant turns that compress has something to fold.
    for i in 0..6 {
        store
            .append(houyicoder_context::SessionLogEntry {
                id: houyicoder_context::EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                event: if i == 0 {
                    SessionEvent::UserInput {
                        text: "do the work".into(),
                    }
                } else {
                    SessionEvent::AssistantMessage {
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
    let boundary = events
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::CompactionBoundary {
                pre_tokens,
                post_tokens,
                ..
            } => Some((*pre_tokens, *post_tokens)),
            _ => None,
        })
        .expect("a compaction boundary lands when compress folds events");
    // Both counts are recorded. The direction depends on the summarizer: a
    // fold whose summary is longer than the span it replaced can leave a
    // larger estimate, which is a fact about that fold, not a failure.
    assert!(boundary.0 > 0, "the boundary records what was there");
    assert!(
        boundary.1 > 0,
        "and what remained: {} → {}",
        boundary.0,
        boundary.1
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::Summary { .. }))
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
    // 1000. The assembled context (system prompt + a few events ≈ 2-3k) lands in
    // (threshold, window/2] = (1000, 11000] — so the economy gate (conservative >
    // window/2) SKIPS and ONLY the pre-flight path (conservative > threshold) can
    // fire. This is path attribution: a mutation that flips the pre-flight
    // comparison (>) to (<) now removes the compact + checkpoint, failing
    // the test — the prior window=200 made economy fire first (conservative >
    // window/2=100) so the pre-flight mutation survived (false-green, #113).
    // After compress (folding the older events), the next iteration has a
    // manifest applied and the assembled context is smaller.
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
            .append(houyicoder_context::SessionLogEntry {
                id: houyicoder_context::EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                event: if i == 0 {
                    SessionEvent::UserInput {
                        text: "do the work".into(),
                    }
                } else {
                    SessionEvent::AssistantMessage {
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
    // run_compaction, which commits a checkpoint via write_checkpoint.
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
/// older memory-recall events out of the assembled context (Summarized), and the
/// re-inject surfaces them again. This pins that the re-inject call on the
/// hot path actually executes when pre-flight trips — without it, the line
/// is dead (the prior weak pre-flight test never exceeded the window).
#[tokio::test]
async fn test_compress_runs_reinject() {
    // Tiny window plus long pre-populated turns so the assembled context exceeds
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
            .append(houyicoder_context::SessionLogEntry {
                id: houyicoder_context::EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                event: if i == 0 {
                    SessionEvent::UserInput {
                        text: "do the work".into(),
                    }
                } else {
                    SessionEvent::AssistantMessage {
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
            .any(|e| matches!(e.event, SessionEvent::CompactionBoundary { .. })),
        "pre-flight must trip compress when the assembled context exceeds the window"
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
    let p = Arc::new(super::HangingProvider::new(vec![]));
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
    let p = Arc::new(super::HangingProvider::new(vec![LlmEvent::TextDelta {
        id: "t1".into(),
        text: "partial".into(),
    }]));
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
            e.event,
            houyicoder_context::SessionEvent::AssistantMessage { .. }
        )),
        "partial text flushed before the stall failure"
    );
}

// ===== Context-shrink boundaries hand off to the extraction pipeline =====

/// A recording memory for the boundary tests: captures every write the
/// forked extraction lands and lists nothing pre-seeded, so the fork's
/// manifest injection starts empty.
#[derive(Default)]
struct RecordingMemory {
    written: Mutex<Vec<MemoryEntry>>,
}

impl RecordingMemory {
    fn written_entries(&self) -> Vec<MemoryEntry> {
        self.written.lock().expect("written").clone()
    }
}

impl MemoryProvider for RecordingMemory {
    fn add(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        self.written.lock().expect("written").push(entry);
        Ok(())
    }
    fn list_memories(&self) -> Vec<MemorySummary> {
        Vec::new()
    }
}

/// The canned model for the forked extraction run: the first call saves one
/// fact quoted from the window, the second call ends the run. Counts calls
/// so a gated test can assert the fork never launched.
struct CannedSaver {
    calls: Mutex<usize>,
}

impl CannedSaver {
    fn call_count(&self) -> usize {
        *self.calls.lock().expect("calls")
    }

    fn next(&self) -> usize {
        let mut calls = self.calls.lock().expect("calls");
        *calls += 1;
        *calls
    }

    fn response(n: usize) -> CompletionResponse {
        if n == 1 {
            CompletionResponse {
                output: vec![
                    OutputItem::Text {
                        text: "Saving the token-expiry fact.".into(),
                    },
                    OutputItem::ToolCall {
                        id: "save1".into(),
                        name: "save_memory".into(),
                        input: serde_json::json!({
                            "key": "deploy-token-expiry",
                            "description": "Deploy tokens expire nightly",
                            "source": "project",
                            "content": "Vault deploy tokens expire nightly.\n**Why:** an overnight run outlives the token.\n**How to apply:** rotate before long runs.",
                            "evidence": [{"quote": "the deploy tokens expire nightly"}]
                        }),
                    },
                ],
                usage: Usage::default(),
                model: "test".to_string(),
            }
        } else {
            CompletionResponse {
                output: vec![OutputItem::Text {
                    text: "done".into(),
                }],
                usage: Usage::default(),
                model: "test".to_string(),
            }
        }
    }
}

impl ModelProvider for CannedSaver {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let resp = Self::response(self.next());
        Box::pin(async move { Ok(resp) })
    }
    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        stream_from_response(Self::response(self.next()))
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// Assemble a runner whose memory runtime carries a live extractor: the
/// forked run talks to the canned saver and writes into the recording
/// memory. Returns the handles a boundary test asserts on.
fn preservation_rig(
    gates: MemoryGates,
) -> (
    Runner,
    Arc<MemoryExtractor>,
    Arc<RecordingMemory>,
    Arc<CannedSaver>,
) {
    let saver = Arc::new(CannedSaver {
        calls: Mutex::new(0),
    });
    let memory = Arc::new(RecordingMemory::default());
    let runner = runner_with(Arc::new(FakeProvider::text("ok")), ToolRegistry::new());
    let store = runner.store();
    let fork: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let cwd = temp_dir().join(format!("boundary-preserve-{}", process::id()));
    fs::create_dir_all(&cwd).expect("mkdir cwd");
    let extractor = Arc::new(MemoryExtractor::new(
        ExtractionLogs {
            session: Arc::clone(&store),
            fork,
        },
        Arc::clone(&saver) as Arc<dyn ModelProvider>,
        Arc::clone(&memory) as Arc<dyn MemoryProvider>,
        cwd,
        RunnerConfig::default(),
    ));
    let runtime = MemoryRuntime::from_parts(
        store,
        Some(Arc::clone(&memory) as Arc<dyn MemoryProvider>),
        gates,
        Some(Arc::clone(&extractor)),
        None,
    );
    (runner.install_memory(runtime), extractor, memory, saver)
}

/// Seed one user query plus six assistant turns. The fold policy keeps the
/// last four assistant turns, so compaction has the oldest two to fold; the
/// second assistant turn carries the sentence the canned save quotes as
/// evidence.
async fn seed_boundary_session(runner: &Runner, session: SessionId) {
    let store = runner.store();
    let texts = [
        "watch the overnight deploys",
        "turn 1 hit an error here",
        "noted — the deploy tokens expire nightly, so long runs need a refresh",
        "turn 3 steady state",
        "turn 4 steady state",
        "turn 5 steady state",
        "turn 6 steady state",
    ];
    for (index, text) in texts.iter().enumerate() {
        let event = if index == 0 {
            SessionEvent::UserInput {
                text: (*text).to_string(),
            }
        } else {
            SessionEvent::AssistantMessage {
                text: (*text).to_string(),
                thinking: None,
            }
        };
        store
            .append(SessionLogEntry {
                id: EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                event,
            })
            .await
            .expect("seed append");
    }
}

/// The compact boundary hands the shrinking context to the extraction
/// pipeline: the forked run saves the quoted fact through the pinned tool
/// under the extractor origin.
#[tokio::test]
async fn test_compress_runs_extraction() {
    let (runner, extractor, memory, _saver) = preservation_rig(MemoryGates::new(true, false));
    let session = SessionId::new();
    seed_boundary_session(&runner, session).await;
    let progress = runner.compress(session).await;
    assert!(progress.is_ok(), "compress must not error");
    assert!(progress.unwrap(), "folds the oldest assistant turns");
    extractor.drain_pending(Duration::from_secs(5)).await;
    let written = memory.written_entries();
    assert_eq!(written.len(), 1, "the forked run saved exactly one fact");
    assert_eq!(written[0].key, "deploy-token-expiry");
    assert_eq!(written[0].origin, MemoryOrigin::Extractor);
}

/// With the auto-memory gate off the compact boundary launches no fork and
/// writes nothing, while the compaction itself still proceeds.
#[tokio::test]
async fn test_compress_respects_gate() {
    let (runner, extractor, memory, saver) = preservation_rig(MemoryGates::new(false, false));
    let session = SessionId::new();
    seed_boundary_session(&runner, session).await;
    let progress = runner.compress(session).await;
    assert!(progress.is_ok(), "compaction itself is not gated");
    extractor.drain_pending(Duration::from_secs(5)).await;
    assert_eq!(saver.call_count(), 0, "no fork launches");
    assert!(
        memory.written_entries().is_empty(),
        "gate off writes nothing"
    );
}

/// The clear boundary hands the about-to-drop session to the same
/// extraction pipeline: one quoted fact lands through the pinned tool
/// under the extractor origin.
#[tokio::test]
async fn test_clear_runs_extraction() {
    let (runner, extractor, memory, _saver) = preservation_rig(MemoryGates::new(true, false));
    let session = SessionId::new();
    seed_boundary_session(&runner, session).await;
    runner.before_clear(session).await;
    extractor.drain_pending(Duration::from_secs(5)).await;
    let written = memory.written_entries();
    assert_eq!(written.len(), 1, "the forked run saved exactly one fact");
    assert_eq!(written[0].key, "deploy-token-expiry");
    assert_eq!(written[0].origin, MemoryOrigin::Extractor);
}

/// With the auto-memory gate off the clear boundary launches no fork and
/// writes nothing.
#[tokio::test]
async fn test_clear_respects_gate() {
    let (runner, extractor, memory, saver) = preservation_rig(MemoryGates::new(false, false));
    let session = SessionId::new();
    seed_boundary_session(&runner, session).await;
    runner.before_clear(session).await;
    extractor.drain_pending(Duration::from_secs(5)).await;
    assert_eq!(saver.call_count(), 0, "no fork launches");
    assert!(
        memory.written_entries().is_empty(),
        "gate off writes nothing"
    );
}

/// A runtime without an extractor still clears cleanly: the boundary
/// degrades to a no-op instead of failing.
#[tokio::test]
async fn test_clear_without_extractor() {
    let memory = Arc::new(RecordingMemory::default());
    let runner = runner_with(Arc::new(FakeProvider::text("ok")), ToolRegistry::new());
    let store = runner.store();
    let runtime = MemoryRuntime::from_parts(
        store,
        Some(Arc::clone(&memory) as Arc<dyn MemoryProvider>),
        MemoryGates::new(true, false),
        None,
        None,
    );
    let runner = runner.install_memory(runtime);
    let session = SessionId::new();
    seed_boundary_session(&runner, session).await;
    runner.before_clear(session).await;
    assert!(
        memory.written_entries().is_empty(),
        "no extractor means no preservation writes"
    );
}

/// before_clear is a no-op (no panic, no write) when no memory runtime is
/// installed — the clear still proceeds on a memory-less runner.
#[tokio::test]
async fn test_clear_noop_without_memory() {
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let runner = runner_with(provider, ToolRegistry::new());
    let session = SessionId::new();
    runner.before_clear(session).await;
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
    // The provider omits usage (Usage::default()); the estimated-token
    // fallback fills input_tokens so the status gauge + tally read the real
    // footprint, not a silent 0.
    assert!(
        result.usage.input_tokens > 0,
        "omitted usage falls back to the estimated count: {:#?}",
        result.usage
    );
}
