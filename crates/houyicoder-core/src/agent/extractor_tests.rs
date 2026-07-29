use super::*;
use crate::agent::{Runner, ToolRegistry};
use houyicoder_api::live::{LiveEvent, LiveSink, MemorySavedKind};
use houyicoder_context::{MemoryEntry, MemorySummary};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, InputItem, LlmEvent, ModelCapabilities, ModelSettings,
    OutputItem, ProviderError, Usage,
};
use houyicoder_session::SessionStore;
use std::collections::HashSet;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU32;

use houyicoder_api::provider::stream_from_response;
use houyicoder_async::{PFut, PStream};

/// A recording memory so the test asserts writes landed.
struct RecordingMemory {
    written: StdMutex<Vec<MemoryEntry>>,
}
impl MemoryProvider for RecordingMemory {
    fn recall(&self, _q: &str, _b: usize, _surfaced: &HashSet<String>) -> Vec<MemoryEntry> {
        Vec::new()
    }
    fn add(&self, e: MemoryEntry) -> Result<(), houyicoder_context::MemoryError> {
        self.written.lock().expect("w").push(e);
        Ok(())
    }
}

/// A recording live sink capturing MemorySaved events so the test asserts the
/// extractor fired one per pass. Other events (deltas) are ignored — the
/// forked runner's live sink is None, so none fire here anyway.
struct RecordingSink(StdMutex<Vec<LiveEvent>>);
impl RecordingSink {
    fn new() -> (LiveSink, std::sync::Arc<RecordingSink>) {
        let inner = std::sync::Arc::new(RecordingSink(StdMutex::new(Vec::new())));
        let inner_clone = std::sync::Arc::clone(&inner);
        let sink: LiveSink = std::sync::Arc::new(move |ev: &LiveEvent| {
            inner_clone.0.lock().expect("sink").push(ev.clone());
        });
        (sink, inner)
    }
    fn memory_saved(&self) -> Vec<(u32, MemorySavedKind)> {
        self.0
            .lock()
            .expect("sink")
            .iter()
            .filter_map(|ev| match ev {
                LiveEvent::MemorySaved { count, kind } => Some((*count, *kind)),
                _ => None,
            })
            .collect()
    }
}

/// A scripted provider: call 1 emits save_memory, call 2 final text.
struct FakeProvider {
    calls: StdMutex<usize>,
}
impl ModelProvider for FakeProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let mut c = self.calls.lock().expect("c");
        *c += 1;
        let n = *c;
        drop(c);
        Box::pin(async move { Ok(scripted(n)) })
    }
    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let mut c = self.calls.lock().expect("c");
        *c += 1;
        let n = *c;
        drop(c);
        stream_from_response(scripted(n))
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}
fn scripted(n: usize) -> CompletionResponse {
    if n % 2 == 1 {
        CompletionResponse {
            output: vec![
                OutputItem::Text {
                    text: "saving".into(),
                },
                OutputItem::ToolCall {
                    id: "s1".into(),
                    name: "save_memory".into(),
                    input: serde_json::json!({
                        "key": "k", "description": "d",
                        "source": "feedback", "content": "c"
                    }),
                },
            ],
            usage: Usage::default(),
            model: "test".into(),
        }
    } else {
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }
    }
}

/// A provider that always errors, to drive the no-advance-on-error path.
struct ErrorProvider;
impl ModelProvider for ErrorProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Auth) })
    }
    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        stream_from_response(CompletionResponse {
            output: vec![],
            usage: Usage::default(),
            model: "test".into(),
        })
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A provider for the Runner-fires-extractor test: call 1 (the main run)
/// returns a final text so the main loop reaches FinalOutput; call 2 (the
/// forked extraction) emits save_memory; call 3+ final text. Distinct from
/// FakeProvider so the main run's first call is final, not save.
struct MainFinalProvider {
    calls: StdMutex<usize>,
}
impl ModelProvider for MainFinalProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let mut c = self.calls.lock().expect("c");
        *c += 1;
        let n = *c;
        drop(c);
        Box::pin(async move { Ok(scripted_main(n)) })
    }
    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let mut c = self.calls.lock().expect("c");
        *c += 1;
        let n = *c;
        drop(c);
        stream_from_response(scripted_main(n))
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}
fn scripted_main(n: usize) -> CompletionResponse {
    // n=1: main run final text. n=2: forked save_memory. n>=3: forked final.
    if n == 2 {
        CompletionResponse {
            output: vec![
                OutputItem::Text {
                    text: "saving".into(),
                },
                OutputItem::ToolCall {
                    id: "s1".into(),
                    name: "save_memory".into(),
                    input: serde_json::json!({
                        "key": "k", "description": "d",
                        "source": "feedback", "content": "c"
                    }),
                },
            ],
            usage: Usage::default(),
            model: "test".into(),
        }
    } else {
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }
    }
}

fn extractor(provider: Arc<dyn ModelProvider>) -> (Arc<MemoryExtractor>, Arc<RecordingMemory>) {
    let memory = Arc::new(RecordingMemory {
        written: StdMutex::new(Vec::new()),
    });
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let cwd = std::env::temp_dir().join(format!("extractor-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("mkdir");
    let config = RunnerConfig {
        max_turns: 5,
        ..RunnerConfig::default()
    };
    let ext = Arc::new(MemoryExtractor::new(
        store,
        provider,
        Arc::clone(&memory) as Arc<dyn MemoryProvider>,
        cwd,
        config,
    ));
    (ext, memory)
}

/// Build a simple conversation prefix: user asks, assistant answers.
fn conversation() -> Vec<TurnEvent> {
    let session = houyicoder_context::SessionId::new();
    vec![
        TurnEvent {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::UserInput {
                text: "remember to keep responses terse".into(),
            },
        },
        TurnEvent {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: "got it".into(),
                thinking: None,
            },
        },
    ]
}

/// A clean conversation: the fork runs and the cursor advances to the
/// last message on success.
#[tokio::test]
async fn test_extract_advances_cursor_success() {
    let (ext, memory) = extractor(Arc::new(FakeProvider {
        calls: StdMutex::new(0),
    }));
    let (sink, recording) = RecordingSink::new();
    ext.set_notify_sink(sink);
    let msgs = conversation();
    let outcome = ext.run_extraction_once(&msgs).await.expect("run ok");
    assert!(
        matches!(outcome, ExtractOutcome::Extracted(_)),
        "clean run extracts"
    );
    assert_eq!(
        memory.written.lock().expect("w").len(),
        1,
        "forked agent saved one memory"
    );
    assert_eq!(
        *ext.cursor.lock().expect("cursor"),
        Some(msgs.last().expect("last").id),
        "cursor advances to last message on success"
    );
    // The fork wrote one memory, so the sink fires one Extracted notice.
    assert_eq!(
        recording.memory_saved(),
        vec![(1, MemorySavedKind::Extracted)],
        "a successful fork fires one Extracted memory-saved notice"
    );
}

/// When the main agent already emitted a save_memory call in this turn
/// range, the fork is skipped and the cursor still advances past the
/// range so the next run does not re-scan it.
#[tokio::test]
async fn test_extract_skips_main_saved() {
    let (ext, memory) = extractor(Arc::new(FakeProvider {
        calls: StdMutex::new(0),
    }));
    let (sink, recording) = RecordingSink::new();
    ext.set_notify_sink(sink);
    // The prefix already contains a save_memory tool call (the main agent
    // saved this turn) — mutual exclusion must skip the fork.
    let mut msgs = conversation();
    msgs.push(TurnEvent {
        id: EventId::new(),
        session: msgs[0].session,
        ts: 0,
        prev_hash: None,
        kind: TurnEventKind::ToolCall {
            call_id: "main-save".into(),
            tool: "save_memory".into(),
            input: serde_json::json!({
                "key": "k", "description": "d",
                "source": "feedback", "content": "c"
            }),
        },
    });
    let outcome = ext.run_extraction_once(&msgs).await.expect("run ok");
    assert!(
        matches!(outcome, ExtractOutcome::Skipped { .. }),
        "must skip when main agent already saved"
    );
    assert!(
        memory.written.lock().expect("w").is_empty(),
        "no fork run, no write"
    );
    assert_eq!(
        *ext.cursor.lock().expect("cursor"),
        Some(msgs.last().expect("last").id),
        "cursor still advances on skip"
    );
    // The Skipped path is the one the user directly triggered (the main agent
    // saved), so it still fires a notice — one Extracted, count = the saves
    // the main agent emitted.
    assert_eq!(
        recording.memory_saved(),
        vec![(1, MemorySavedKind::Extracted)],
        "the skipped (main-agent-saved) path still fires an Extracted notice"
    );
}

/// On a provider error the cursor does NOT advance — the errored range
/// is reconsidered on the next pass.
#[tokio::test]
async fn test_extract_keeps_cursor_error() {
    let (ext, _memory) = extractor(Arc::new(ErrorProvider));
    let msgs = conversation();
    let result = ext.run_extraction_once(&msgs).await;
    let err = result.expect_err("erroring provider must error the fork");
    assert!(
        err.to_string().contains("fork hit max turns"),
        "fork max-turns error message: {err}"
    );
    assert!(
        ext.cursor.lock().expect("cursor").is_none(),
        "cursor must not advance on error"
    );
}

/// count_messages_since counts all model-visible messages when the cursor
/// is None (fresh process) and when the cursor id is not in the messages
/// (compaction removed it) — never 0, which would disable extraction.
#[test]
fn test_count_since_fallbacks_missing() {
    let msgs = conversation();
    assert_eq!(
        count_messages_since(&msgs, None),
        2,
        "fresh cursor counts all model-visible messages"
    );
    let foreign = EventId::new(); // not in msgs
    assert_eq!(
        count_messages_since(&msgs, Some(&foreign)),
        2,
        "missing cursor id falls back to counting all, not 0"
    );
    let mid = msgs[0].id;
    assert_eq!(
        count_messages_since(&msgs, Some(&mid)),
        1,
        "cursor at first message counts the one after"
    );
}

/// has_memory_writes_since detects a save_memory call after the cursor.
#[test]
fn test_has_writes_detects_save() {
    let msgs = conversation();
    assert!(
        !has_memory_writes_since(&msgs, None),
        "clean conversation has no save_memory call"
    );
    let mut msgs = msgs;
    msgs.push(TurnEvent {
        id: EventId::new(),
        session: msgs[0].session,
        ts: 0,
        prev_hash: None,
        kind: TurnEventKind::ToolCall {
            call_id: "c".into(),
            tool: "save_memory".into(),
            input: serde_json::json!({}),
        },
    });
    assert!(
        has_memory_writes_since(&msgs, None),
        "save_memory call detected"
    );
    // Cursor at the save_memory call: scan starts after it → false.
    let save_id = msgs.last().expect("last").id;
    assert!(
        !has_memory_writes_since(&msgs, Some(&save_id)),
        "scan after the save call finds nothing"
    );
}

/// extract_memories is fire-and-forget: it returns immediately and the
/// forked run lands on a spawned task. drain_pending waits for the
/// handle so the test asserts the fork actually completed + wrote.
#[tokio::test]
async fn test_extract_memories_spawns_drain() {
    let (ext, memory) = extractor(Arc::new(FakeProvider {
        calls: StdMutex::new(0),
    }));
    ext.extract_memories(conversation());
    // Returns immediately; the fork runs on a spawned task.
    ext.drain_pending(Duration::from_secs(5)).await;
    assert_eq!(
        memory.written.lock().expect("w").len(),
        1,
        "fork wrote after drain"
    );
    assert!(
        ext.cursor.lock().expect("cursor").is_some(),
        "cursor advanced after drain"
    );
    assert!(
        ext.in_flight.lock().expect("in_flight").is_empty(),
        "drain clears the in-flight set"
    );
}

/// drain_pending is a no-op when nothing is in flight — returns
/// immediately, no panic.
#[tokio::test]
async fn test_drain_pending_noop_empty() {
    let (ext, _memory) = extractor(Arc::new(FakeProvider {
        calls: StdMutex::new(0),
    }));
    ext.drain_pending(Duration::from_secs(1)).await;
    assert!(
        ext.in_flight.lock().expect("in_flight").is_empty(),
        "nothing in flight"
    );
}

/// When a fork is in-flight, a second trigger coalesces: the new context
/// is stashed (overwriting any older stash) and NO new task is spawned.
/// Deterministic — arms in_progress manually rather than racing a real
/// in-flight fork.
#[tokio::test]
async fn test_extract_memories_coalesces_flight() {
    let (ext, _memory) = extractor(Arc::new(FakeProvider {
        calls: StdMutex::new(0),
    }));
    *ext.in_progress.lock().expect("in_progress") = true;
    let before = ext.in_flight.lock().expect("in_flight").len();
    ext.extract_memories(conversation());
    assert!(
        ext.pending_context.lock().expect("pending").is_some(),
        "second call stashed, not spawned"
    );
    assert_eq!(
        ext.in_flight.lock().expect("in_flight").len(),
        before,
        "no new task spawned while in-flight"
    );
}

/// The fire-and-forget body picks up a stashed trailing context in its
/// finally: the initial pass runs, then the trailing pass runs, then
/// in_progress clears. Deterministic — pre-stashes the context + runs
/// the body directly (no spawn, no race). Two writes land (initial +
/// trailing), the cursor advances past both, and in_progress ends false.
#[tokio::test]
async fn test_run_extraction_picks_trailing() {
    let (ext, memory) = extractor(Arc::new(FakeProvider {
        calls: StdMutex::new(0),
    }));
    *ext.pending_context.lock().expect("pending") = Some(conversation());
    *ext.in_progress.lock().expect("in_progress") = true;
    Arc::clone(&ext).run_extraction(conversation(), false).await;
    assert_eq!(
        memory.written.lock().expect("w").len(),
        2,
        "initial + trailing both wrote"
    );
    assert!(
        !*ext.in_progress.lock().expect("in_progress"),
        "in_progress cleared after the chain"
    );
    assert!(
        ext.pending_context.lock().expect("pending").is_none(),
        "pending drained"
    );
}

/// The Runner fires extract_memories at query-loop end (FinalOutput, no
/// tool calls). The main run reaches FinalOutput; the spawned forked
/// extraction then writes a memory. This is the stop-hook trigger
/// wiring — the piece that makes the extractor auto-fire (not just
/// test-driven). Drains the spawned fork before asserting.
#[tokio::test]
async fn test_runner_fires_extractor_final() {
    let provider = Arc::new(MainFinalProvider {
        calls: StdMutex::new(0),
    });
    let memory = Arc::new(RecordingMemory {
        written: StdMutex::new(Vec::new()),
    });
    let main_store: Arc<dyn SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let ephemeral: Arc<dyn SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let cwd = std::env::temp_dir().join(format!("runner-fire-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("mkdir");
    let ext = Arc::new(MemoryExtractor::new(
        ephemeral,
        Arc::clone(&provider) as Arc<dyn ModelProvider>,
        Arc::clone(&memory) as Arc<dyn MemoryProvider>,
        cwd.clone(),
        RunnerConfig {
            max_turns: 5,
            ..RunnerConfig::default()
        },
    ));
    let runner = Runner::with_shared_store(
        main_store,
        Arc::clone(&provider) as Arc<dyn ModelProvider>,
        ToolRegistry::new(),
        RunnerConfig {
            max_turns: 5,
            ..RunnerConfig::default()
        },
    )
    .with_memory(Arc::clone(&memory) as Arc<dyn MemoryProvider>)
    .with_extractor(Arc::clone(&ext));
    let session = houyicoder_context::SessionId::new();
    let result = runner
        .run(session, "remember to keep responses terse".into())
        .await
        .expect("run");
    assert!(
        matches!(result.outcome, crate::agent::RunOutcome::FinalOutput(_)),
        "main run reaches FinalOutput"
    );
    // FinalOutput fired the extractor (fire-and-forget). Drain the fork.
    ext.drain_pending(Duration::from_secs(5)).await;
    assert_eq!(
        memory.written.lock().expect("w").len(),
        1,
        "forked extraction wrote a memory after FinalOutput"
    );
    std::fs::remove_dir_all(&cwd).ok();
}

/// extract_memories short-circuits when there are no new messages since
/// the cursor (cursor at the last message) — no spawn, no stash. Pins
/// the cheap pre-check so a re-emitted FinalOutput does not re-spawn.
#[tokio::test]
async fn test_extract_skips_no_new() {
    let (ext, _memory) = extractor(Arc::new(FakeProvider {
        calls: StdMutex::new(0),
    }));
    let msgs = conversation();
    // Cursor already at the last message → 0 new.
    *ext.cursor.lock().expect("cursor") = Some(msgs.last().expect("last").id);
    ext.extract_memories(msgs);
    assert!(
        ext.in_flight.lock().expect("in_flight").is_empty(),
        "no spawn when no new messages"
    );
    assert!(
        ext.pending_context.lock().expect("pending").is_none(),
        "no stash when no new messages"
    );
}

/// MainFinalProvider.complete returns the scripted response (the runner
/// uses stream, so complete is otherwise dead; this pins it).
#[tokio::test]
async fn test_main_final_complete_returns() {
    let p = MainFinalProvider {
        calls: StdMutex::new(0),
    };
    let r = p
        .complete(CompletionRequest {
            model: "test".into(),
            instructions: String::new(),
            input: vec![],
            tools: vec![],
            settings: ModelSettings::default(),
            cache_breakpoints: Vec::new(),
        })
        .await
        .expect("complete");
    assert_eq!(r.output.len(), 1, "n=1 → final text");
}

/// A memory provider with a pre-seeded existing entry returned by
/// list_memories — the manifest source the forked agent must receive so it
/// dedups instead of re-saving the same fact each turn.
struct SeededMemory {
    existing: Vec<MemorySummary>,
    written: StdMutex<Vec<MemoryEntry>>,
}
impl MemoryProvider for SeededMemory {
    fn recall(&self, _q: &str, _b: usize, _s: &HashSet<String>) -> Vec<MemoryEntry> {
        Vec::new()
    }
    fn add(&self, e: MemoryEntry) -> Result<(), houyicoder_context::MemoryError> {
        self.written.lock().expect("w").push(e);
        Ok(())
    }
    fn list_memories(&self) -> Vec<MemorySummary> {
        self.existing.clone()
    }
}

/// A provider that records the first CompletionRequest it sees on stream so
/// the test can assert the forked agent's actual input carries the manifest.
/// Returns a final-text response (no tool calls) so the fork ends after one
/// provider call.
struct RecordingProvider {
    requests: StdMutex<Vec<CompletionRequest>>,
    calls: StdMutex<usize>,
}
impl ModelProvider for RecordingProvider {
    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let mut c = self.calls.lock().expect("c");
        *c += 1;
        if *c == 1 {
            self.requests.lock().expect("r").push(req.clone());
        }
        drop(c);
        stream_from_response(CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        })
    }
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async {
            Ok(CompletionResponse {
                output: vec![OutputItem::Text {
                    text: "done".into(),
                }],
                usage: Usage::default(),
                model: "test".into(),
            })
        })
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// Effect-level: the forked extractor must receive the existing-memory
/// manifest in its actual provider input so it can dedup (a
/// formatMemoryManifest pre-inject). Asserts the manifest
/// reached the provider's request — not merely that the prompt builder
/// mentions the word "manifest" (string-level would miss the wiring, the
/// blind spot #70 calls out). Without this injection the forked agent is
/// blind to what already exists and re-saves the same fact every turn, which
/// is the "Saved 1 memory" every-turn regression #67 reports.
#[tokio::test]
async fn test_forked_extract_receives_manifest() {
    let memory = Arc::new(SeededMemory {
        existing: vec![MemorySummary {
            key: "build-gate".into(),
            description: "make check must stay green".into(),
            source: houyicoder_context::MemorySource::Project,
            mtime_secs: 0,
            scope: houyicoder_context::MemoryScope::Auto,
            origin: houyicoder_context::MemoryOrigin::Unknown,
        }],
        written: StdMutex::new(Vec::new()),
    });
    let provider = Arc::new(RecordingProvider {
        requests: StdMutex::new(Vec::new()),
        calls: StdMutex::new(0),
    });
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let cwd = std::env::temp_dir().join(format!("manifest-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("mkdir");
    let config = RunnerConfig {
        max_turns: 5,
        ..RunnerConfig::default()
    };
    let prefix = conversation();
    let result = run_forked_extract(
        store,
        provider.clone(),
        Arc::clone(&memory) as Arc<dyn MemoryProvider>,
        &cwd,
        config,
        &prefix,
        Arc::new(AtomicU32::new(0)),
    )
    .await;
    assert!(result.is_ok(), "forked run completes");
    let reqs = provider.requests.lock().expect("r").clone();
    assert!(
        !reqs.is_empty(),
        "forked run made at least one provider call"
    );
    // The extraction prompt + manifest is the last User input item the
    // provider saw. Assert the existing key + manifest heading landed —
    // this is the dedup input that stops the every-turn re-save.
    let user_input = reqs[0]
        .input
        .iter()
        .rev()
        .find_map(|i| match i {
            InputItem::User { content } => Some(content.clone()),
            _ => None,
        })
        .expect("forked request has a user message");
    assert!(
        user_input.contains("build-gate"),
        "forked input must contain the existing memory key, got: {user_input}"
    );
    assert!(
        user_input.contains("Existing memory files"),
        "forked input must carry the manifest heading, got: {user_input}"
    );
}
