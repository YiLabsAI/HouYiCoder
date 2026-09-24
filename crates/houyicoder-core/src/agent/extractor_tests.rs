use super::*;
use crate::agent::memory::{MemoryGates, MemoryRuntime, MutationLog};
use crate::agent::reward_snapshot::RewardSnapshot;
use crate::agent::{Runner, ToolRegistry};
use houyicoder_api::agent_event::{
    AgentEventHandlers, MemoryChangeCausality, MemoryChangeOrigin, MemoryChangedEvent,
};
use houyicoder_context::{MemoryEntry, MemorySummary, SessionId};
use houyicoder_memory::{InMemoryBackend, MarkdownMemoryProvider};
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, InputItem, LlmEvent, ModelCapabilities, ModelSettings,
    OutputItem, ProviderError, Usage,
};
use houyicoder_session::SessionStore;
use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};

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

struct RecordingChanges(StdMutex<Vec<MemoryChangedEvent>>);
impl RecordingChanges {
    fn new() -> (AgentEventHandlers, Arc<Self>) {
        let inner = Arc::new(Self(StdMutex::new(Vec::new())));
        let captured = Arc::clone(&inner);
        let mut events = AgentEventHandlers::default();
        events.set_memory_changed(Arc::new(move |event| {
            captured.0.lock().expect("changes").push(event);
        }));
        (events, inner)
    }

    fn summaries(&self) -> Vec<(usize, MemoryChangeOrigin)> {
        self.0
            .lock()
            .expect("changes")
            .iter()
            .map(|event| (event.changes.len(), event.origin))
            .collect()
    }

    fn causalities(&self) -> Vec<MemoryChangeCausality> {
        self.0
            .lock()
            .expect("changes")
            .iter()
            .map(|event| event.causality)
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
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let quote = first_user_quote(&req);
        let mut c = self.calls.lock().expect("c");
        *c += 1;
        let n = *c;
        drop(c);
        Box::pin(async move { Ok(scripted(n, &quote)) })
    }
    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let quote = first_user_quote(&req);
        let mut c = self.calls.lock().expect("c");
        *c += 1;
        let n = *c;
        drop(c);
        stream_from_response(scripted(n, &quote))
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// Copy the first user message the forked agent saw as the evidence quote,
/// mimicking a model that grounds its save in the window. The fork appends
/// the extraction prompt after the window events, so the first user item is
/// the window's opening user turn whatever the test fed.
fn first_user_quote(req: &CompletionRequest) -> String {
    req.input
        .iter()
        .find_map(|i| match i {
            InputItem::User { content } => Some(content.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn scripted(n: usize, quote: &str) -> CompletionResponse {
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
                        "source": "feedback", "content": "c",
                        "evidence": [{"quote": quote}]
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
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let quote = first_user_quote(&req);
        let mut c = self.calls.lock().expect("c");
        *c += 1;
        let n = *c;
        drop(c);
        Box::pin(async move { Ok(scripted_main(n, &quote)) })
    }
    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let quote = first_user_quote(&req);
        let mut c = self.calls.lock().expect("c");
        *c += 1;
        let n = *c;
        drop(c);
        stream_from_response(scripted_main(n, &quote))
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}
fn scripted_main(n: usize, quote: &str) -> CompletionResponse {
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
                        "source": "feedback", "content": "c",
                        "evidence": [{"quote": quote}]
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
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    extractor_on(provider, store)
}

fn extractor_on(
    provider: Arc<dyn ModelProvider>,
    store: Arc<dyn SessionLog>,
) -> (Arc<MemoryExtractor>, Arc<RecordingMemory>) {
    let memory = Arc::new(RecordingMemory {
        written: StdMutex::new(Vec::new()),
    });
    let cwd = std::env::temp_dir().join(format!("extractor-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("mkdir");
    let config = RunnerConfig {
        max_turns: 5,
        ..RunnerConfig::default()
    };
    let ext = Arc::new(MemoryExtractor::new(
        ExtractionLogs {
            session: Arc::clone(&store),
            fork: store,
        },
        provider,
        Arc::clone(&memory) as Arc<dyn MemoryProvider>,
        cwd,
        config,
    ));
    (ext, memory)
}

#[test]
fn test_seed_never_moves_cursor() {
    let provider = Arc::new(FakeProvider {
        calls: StdMutex::new(0),
    });
    let (ext, _memory) = extractor(Arc::clone(&provider) as Arc<dyn ModelProvider>);
    assert_eq!(ext.cursor(), None, "a fresh extractor consumed nothing");
    let first = EventId::new();
    ext.seed_cursor(first);
    assert_eq!(
        ext.cursor(),
        Some(first),
        "the seed lands on an empty cursor"
    );
    let later = EventId::new();
    ext.seed_cursor(later);
    assert_eq!(
        ext.cursor(),
        Some(first),
        "a cursor already set never moves again"
    );
}

/// Build a simple conversation prefix: user asks, assistant answers.
fn conversation() -> Vec<SessionLogEntry> {
    let session = houyicoder_context::SessionId::new();
    vec![
        SessionLogEntry {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "remember to keep responses terse".into(),
            },
        },
        SessionLogEntry {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::AssistantMessage {
                text: "got it".into(),
                thinking: None,
            },
        },
    ]
}

fn append_event(messages: &mut Vec<SessionLogEntry>, event: SessionEvent) {
    messages.push(SessionLogEntry {
        id: EventId::new(),
        session: messages[0].session,
        ts: 0,
        prev_hash: None,
        event,
    });
}

/// A clean conversation: the fork runs and the cursor advances to the
/// last message on success.
#[tokio::test]
async fn test_extract_advances_cursor_success() {
    let (ext, memory) = extractor(Arc::new(FakeProvider {
        calls: StdMutex::new(0),
    }));
    let (sink, recording) = RecordingChanges::new();
    ext.set_memory_changed_handler(sink.memory_changed_handler());
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
        recording.summaries(),
        vec![(1, MemoryChangeOrigin::AutoMemory)],
        "a successful fork fires one Extracted memory-saved notice"
    );
}

/// A pass whose trigger is still the session's latest prompt belongs to the
/// turn that just completed. Once the user has prompted again while the fork
/// ran, the same pass covers an earlier turn and must say so, or the notice
/// reads as a result of the turn now on screen.
#[tokio::test]
async fn test_extract_labels_change_causality() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let (ext, _memory) = extractor_on(
        Arc::new(FakeProvider {
            calls: StdMutex::new(0),
        }),
        Arc::clone(&store) as Arc<dyn SessionLog>,
    );
    let (sink, recording) = RecordingChanges::new();
    ext.set_memory_changed_handler(sink.memory_changed_handler());
    let mut msgs = conversation();
    // The durable log holds the prompt the window triggers on.
    let opening = msgs[0].clone();
    store.append(opening).await.expect("append the prompt");
    ext.run_extraction_once(&msgs).await.expect("run ok");
    assert_eq!(
        recording.causalities(),
        vec![MemoryChangeCausality::ThisTurn],
        "the trigger is still the latest prompt"
    );

    // A second turn the pass has not consumed yet.
    append_event(
        &mut msgs,
        SessionEvent::UserInput {
            text: "and keep the tests fast".into(),
        },
    );
    append_event(
        &mut msgs,
        SessionEvent::AssistantMessage {
            text: "noted".into(),
            thinking: None,
        },
    );
    // The user prompts again while the fork runs: the frontier moves past
    // this pass's trigger.
    store
        .append(SessionLogEntry {
            id: EventId::new(),
            session: msgs[0].session,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "one more thing".into(),
            },
        })
        .await
        .expect("append the newer prompt");
    ext.run_extraction_once(&msgs).await.expect("run ok");
    assert_eq!(
        recording.causalities(),
        vec![
            MemoryChangeCausality::ThisTurn,
            MemoryChangeCausality::PreviousTurn
        ],
        "a newer prompt moves the notice to an earlier turn"
    );
}

/// A frontier the store cannot answer is reported as an earlier turn rather
/// than claiming the turn on screen.
#[tokio::test]
async fn test_unreadable_frontier_reads_earlier() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let (ext, _memory) = extractor_on(
        Arc::new(FakeProvider {
            calls: StdMutex::new(0),
        }),
        Arc::clone(&store) as Arc<dyn SessionLog>,
    );
    let (sink, recording) = RecordingChanges::new();
    ext.set_memory_changed_handler(sink.memory_changed_handler());
    // The store holds no user input for the window's session, so the
    // comparison has nothing to match.
    ext.run_extraction_once(&conversation())
        .await
        .expect("run ok");
    assert_eq!(
        recording.causalities(),
        vec![MemoryChangeCausality::PreviousTurn],
        "an unreadable frontier never claims the turn on screen"
    );
}

#[tokio::test]
async fn test_unrelated_turn_stays_silent() {
    let provider = Arc::new(FakeProvider {
        calls: StdMutex::new(0),
    });
    let memory_root = std::env::temp_dir().join(format!("extract-repeat-{}", std::process::id()));
    std::fs::create_dir_all(&memory_root).unwrap();
    let memory = Arc::new(MarkdownMemoryProvider::new(memory_root.clone()));
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let extractor = MemoryExtractor::new(
        ExtractionLogs {
            session: Arc::clone(&store),
            fork: store,
        },
        provider,
        memory,
        memory_root.clone(),
        RunnerConfig {
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    let (events, recording) = RecordingChanges::new();
    extractor.set_memory_changed_handler(events.memory_changed_handler());
    let mut messages = conversation();
    extractor.run_extraction_once(&messages).await.unwrap();
    assert_eq!(recording.summaries().len(), 1);
    append_event(
        &mut messages,
        SessionEvent::UserInput {
            text: "which model is active".into(),
        },
    );
    append_event(
        &mut messages,
        SessionEvent::AssistantMessage {
            text: "the configured model is active".into(),
            thinking: None,
        },
    );
    extractor.run_extraction_once(&messages).await.unwrap();
    assert_eq!(
        recording.summaries().len(),
        1,
        "an unrelated turn that repeats the same save stays silent"
    );
    std::fs::remove_dir_all(memory_root).ok();
}

/// When the main agent already emitted a save_memory call in this turn
/// range, the fork is skipped and the cursor still advances past the
/// range so the next run does not re-scan it.
#[tokio::test]
async fn test_extract_skips_main_saved() {
    let (ext, memory) = extractor(Arc::new(FakeProvider {
        calls: StdMutex::new(0),
    }));
    let (sink, recording) = RecordingChanges::new();
    ext.set_memory_changed_handler(sink.memory_changed_handler());
    // The prefix already contains a save_memory tool call (the main agent
    // saved this turn) — mutual exclusion must skip the fork.
    let mut msgs = conversation();
    msgs.push(SessionLogEntry {
        id: EventId::new(),
        session: msgs[0].session,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::ToolCall {
            call_id: "main-save".into(),
            tool: "save_memory".into(),
            input: serde_json::json!({
                "key": "k", "description": "d",
                "source": "feedback", "content": "c"
            }),
        },
    });
    msgs.push(SessionLogEntry {
        id: EventId::new(),
        session: msgs[0].session,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::ToolResult {
            call_id: "main-save".into(),
            output: serde_json::json!({"saved": "k"}),
            duration_ms: 0,
        },
    });
    let outcome = ext.run_extraction_once(&msgs).await.expect("run ok");
    assert!(
        matches!(outcome, ExtractOutcome::Skipped(_)),
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
    // The extractor no longer reconstructs primary saves from the durable
    // log: the main runner records them at call time and drains at the run
    // boundary. So this pass emits no notice from the extractor side.
    assert!(
        recording.summaries().is_empty(),
        "the extractor does not emit primary notices; the runtime drain does"
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
/// in_progress clears. Deterministic — pre-stashes the context + runs the
/// body directly (no spawn, no race). The trailing context extends the
/// same session log as the initial, so the cursor the initial advanced still
/// resolves in it; two writes land (initial + trailing), the cursor
/// advances past both, and in_progress ends false.
#[tokio::test]
async fn test_run_extraction_picks_trailing() {
    let (ext, memory) = extractor(Arc::new(FakeProvider {
        calls: StdMutex::new(0),
    }));
    let session = SessionId::new();
    let first_turn = vec![
        user_entry(session, "first ask"),
        assistant_entry(session, "first answer"),
    ];
    let mut trailing = first_turn.clone();
    push_turn(&mut trailing, session, "second ask", "second answer");
    *ext.pending_context.lock().expect("pending") = Some(trailing);
    *ext.in_progress.lock().expect("in_progress") = true;
    Arc::clone(&ext).run_extraction(first_turn, false).await;
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
        ExtractionLogs {
            session: Arc::clone(&ephemeral),
            fork: ephemeral,
        },
        Arc::clone(&provider) as Arc<dyn ModelProvider>,
        Arc::clone(&memory) as Arc<dyn MemoryProvider>,
        cwd.clone(),
        RunnerConfig {
            max_turns: 5,
            ..RunnerConfig::default()
        },
    ));
    let runner = Runner::new(
        main_store.clone(),
        Arc::clone(&provider) as Arc<dyn ModelProvider>,
        ToolRegistry::new(),
        RunnerConfig {
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    let runtime = MemoryRuntime::from_parts(
        main_store,
        Some(Arc::clone(&memory) as Arc<dyn MemoryProvider>),
        MemoryGates::new(true, true),
        Some(Arc::clone(&ext)),
        None,
    );
    let runner = runner.install_memory(runtime);
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

/// Reward capture off (None) must not skip the extractor. The reward switch
/// suppresses reward only — memory extraction is a memory function, not a
/// reward signal, so it fires regardless. The provider call counter starts
/// at 1 so the fork's first call lands on the save_memory script (n=2),
/// mirroring the main-run-then-fork ordering without running the main loop.
#[tokio::test]
async fn test_extractor_fires_reward_off() {
    let provider = Arc::new(MainFinalProvider {
        calls: StdMutex::new(1),
    });
    let memory = Arc::new(RecordingMemory {
        written: StdMutex::new(Vec::new()),
    });
    let main_store: Arc<dyn SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let ephemeral: Arc<dyn SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let conv = conversation();
    let session = conv[0].session;
    for entry in conv {
        main_store.append(entry).await.expect("append");
    }
    let cwd = std::env::temp_dir().join(format!("reward-off-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("mkdir");
    let ext = Arc::new(MemoryExtractor::new(
        ExtractionLogs {
            session: Arc::clone(&ephemeral),
            fork: ephemeral,
        },
        Arc::clone(&provider) as Arc<dyn ModelProvider>,
        Arc::clone(&memory) as Arc<dyn MemoryProvider>,
        cwd.clone(),
        RunnerConfig {
            max_turns: 5,
            ..RunnerConfig::default()
        },
    ));
    let runtime = MemoryRuntime::from_parts(
        Arc::clone(&main_store),
        Some(Arc::clone(&memory) as Arc<dyn MemoryProvider>),
        MemoryGates::new(true, true),
        Some(Arc::clone(&ext)),
        None,
    );
    runtime
        .fire_background::<fn() -> RewardSnapshot>(session, None)
        .await;
    ext.drain_pending(Duration::from_secs(5)).await;
    assert_eq!(
        memory.written.lock().expect("w").len(),
        1,
        "extractor fires even when reward capture is off"
    );
    std::fs::remove_dir_all(&cwd).ok();
}

/// Extraction remains idle when the cursor covers the latest message.
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

/// run_extraction_once must skip a snapshot the cursor already covers
/// rather than fork: with no query turn to read the fork would have nothing
/// eligible to cite.
#[tokio::test]
async fn test_zero_window_skips_fork() {
    let provider = Arc::new(FakeProvider {
        calls: StdMutex::new(0),
    });
    let (ext, _memory) = extractor(Arc::clone(&provider) as Arc<dyn ModelProvider>);
    let msgs = conversation();
    *ext.cursor.lock().expect("cursor") = Some(msgs.last().expect("last").id);
    let outcome = ext.run_extraction_once(&msgs).await.expect("run ok");
    assert!(
        matches!(outcome, ExtractOutcome::Skipped(ExtractSkip::NoQueryTurn)),
        "a cursor that covers the snapshot skips with no query turn"
    );
    assert_eq!(*provider.calls.lock().expect("calls"), 0, "no fork ran");
}

/// The trailing drain reaches run_extraction_once without the fire-path
/// pre-check: a stashed context the cursor already covers must not fork.
#[tokio::test]
async fn test_trailing_zero_window() {
    let provider = Arc::new(FakeProvider {
        calls: StdMutex::new(0),
    });
    let (ext, _memory) = extractor(Arc::clone(&provider) as Arc<dyn ModelProvider>);
    let msgs = conversation();
    *ext.cursor.lock().expect("cursor") = Some(msgs.last().expect("last").id);
    *ext.pending_context.lock().expect("pending") = Some(msgs.clone());
    *ext.in_progress.lock().expect("in_progress") = true;
    Arc::clone(&ext).run_extraction(msgs, false).await;
    assert_eq!(
        *provider.calls.lock().expect("calls"),
        0,
        "neither pass forked"
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

/// MainFinalProvider returns its scripted completion response.
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
    let window = ExactExtractionWindow::from_unconsumed(
        ExactExtractionWindow::unconsumed(&prefix, None).expect("fresh cursor locates"),
    )
    .expect("a turn is present");
    let result = run_forked_extract(
        store,
        provider.clone(),
        Arc::clone(&memory) as Arc<dyn MemoryProvider>,
        &cwd,
        config,
        &window,
        Arc::new(MutationLog::new()),
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
    assert!(
        user_input.contains("the complete evidence window"),
        "the fork must be told its whole input is eligible: {user_input}"
    );
}

/// One durable event in a test history.
fn entry(session: SessionId, event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        event,
    }
}

fn user_entry(session: SessionId, text: &str) -> SessionLogEntry {
    entry(
        session,
        SessionEvent::UserInput {
            text: text.to_string(),
        },
    )
}

fn assistant_entry(session: SessionId, text: &str) -> SessionLogEntry {
    entry(
        session,
        SessionEvent::AssistantMessage {
            text: text.to_string(),
            thinking: None,
        },
    )
}

/// Append one query turn so a history can grow a turn at a time.
fn push_turn(messages: &mut Vec<SessionLogEntry>, session: SessionId, ask: &str, answer: &str) {
    messages.push(user_entry(session, ask));
    messages.push(assistant_entry(session, answer));
}

/// Records what each forked request carried and answers the first call with
/// a save_memory write, so a test reads back the exact evidence the forked
/// agent could cite.
struct EvidenceProbe {
    requests: StdMutex<Vec<String>>,
    calls: StdMutex<usize>,
}

impl EvidenceProbe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: StdMutex::new(Vec::new()),
            calls: StdMutex::new(0),
        })
    }

    fn record(&self, req: CompletionRequest) -> usize {
        let mut calls = self.calls.lock().expect("calls");
        *calls += 1;
        let n = *calls;
        drop(calls);
        let input = serde_json::to_string(&req.input).unwrap_or_default();
        self.requests
            .lock()
            .expect("requests")
            .push(format!("{}\n{input}", req.instructions));
        n
    }

    fn first_request(&self) -> String {
        self.requests
            .lock()
            .expect("requests")
            .first()
            .expect("the fork made a request")
            .clone()
    }

    fn call_count(&self) -> usize {
        *self.calls.lock().expect("calls")
    }
}

impl ModelProvider for EvidenceProbe {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let n = self.record(req);
        Box::pin(async move { Ok(probe_response(n)) })
    }
    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let n = self.record(req);
        stream_from_response(probe_response(n))
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// The first answer writes a memory keyed on a fact only an older turn
/// states; later answers end the forked run.
fn probe_response(n: usize) -> CompletionResponse {
    let output = if n == 1 {
        vec![OutputItem::ToolCall {
            id: "probe-save".into(),
            name: "save_memory".into(),
            input: serde_json::json!({
                "key": "ashford-ledger-retention",
                "description": "The Ashford ledger stays for seven years",
                "source": "feedback",
                "content": "Retain the Ashford ledger for seven years."
            }),
        }]
    } else {
        vec![OutputItem::Text {
            text: "done".into(),
        }]
    };
    CompletionResponse {
        output,
        usage: Usage::default(),
        model: "test".into(),
    }
}

/// The forked extractor reads only the turn that triggered the pass. A fact
/// from an already-consumed turn is not evidence for this pass, so it never
/// reaches the forked request and a write keyed on it has nothing to cite.
#[tokio::test]
async fn test_fork_sees_new_turn() {
    let probe = EvidenceProbe::new();
    let (ext, _memory) = extractor(Arc::clone(&probe) as Arc<dyn ModelProvider>);
    let session = SessionId::new();
    let mut messages = vec![
        user_entry(session, "Retain the Ashford ledger for seven years."),
        assistant_entry(session, "The Ashford ledger stays for seven years."),
    ];
    ext.seed_cursor(messages.last().expect("last").id);
    push_turn(
        &mut messages,
        session,
        "Add the sidebar toggle.",
        "The sidebar toggle is in.",
    );

    ext.run_extraction_once(&messages).await.expect("run ok");

    let seen = probe.first_request();
    assert!(
        seen.contains("sidebar"),
        "the triggering turn reaches the fork: {seen}"
    );
    assert!(
        !seen.contains("Ashford"),
        "a consumed turn is not evidence for this pass: {seen}"
    );
}

/// A resumed session seeds the cursor to the restored tail, so the first
/// pass reads the turn that arrived after the restore and nothing from the
/// history the restore carried in.
#[tokio::test]
async fn test_resume_reads_new_turn() {
    let probe = EvidenceProbe::new();
    let (ext, _memory) = extractor(Arc::clone(&probe) as Arc<dyn ModelProvider>);
    let session = SessionId::new();
    let mut messages = vec![
        user_entry(session, "Retain the Ashford ledger for seven years."),
        assistant_entry(session, "The Ashford ledger stays for seven years."),
        user_entry(session, "Which host runs the nightly build?"),
        assistant_entry(session, "The nightly build runs on the spare host."),
    ];
    ext.seed_cursor(messages.last().expect("last").id);
    push_turn(
        &mut messages,
        session,
        "Add the sidebar toggle.",
        "The sidebar toggle is in.",
    );

    ext.run_extraction_once(&messages).await.expect("run ok");

    let seen = probe.first_request();
    assert!(
        seen.contains("sidebar"),
        "the post-restore turn reaches the fork: {seen}"
    );
    assert!(
        !seen.contains("Ashford"),
        "restored history is not evidence: {seen}"
    );
    assert!(
        !seen.contains("nightly"),
        "a restored turn before the seed is not evidence: {seen}"
    );
}

/// A cursor that names an event the snapshot does not hold skips the pass
/// and re-seeds to the snapshot tail, so the fork never widens to the full
/// history and the next pass has a located range.
#[tokio::test]
async fn test_lost_cursor_skips_fork() {
    let probe = EvidenceProbe::new();
    let (ext, _memory) = extractor(Arc::clone(&probe) as Arc<dyn ModelProvider>);
    let session = SessionId::new();
    let mut messages = vec![
        user_entry(session, "Retain the Ashford ledger for seven years."),
        assistant_entry(session, "The Ashford ledger stays for seven years."),
    ];
    push_turn(
        &mut messages,
        session,
        "Add the sidebar toggle.",
        "The sidebar toggle is in.",
    );
    *ext.cursor.lock().expect("cursor") = Some(EventId::new());
    let outcome = ext.run_extraction_once(&messages).await.expect("run ok");
    assert!(
        matches!(outcome, ExtractOutcome::Skipped(ExtractSkip::CursorLost)),
        "a lost cursor skips rather than widening to the full history"
    );
    assert_eq!(probe.call_count(), 0, "the fork did not run");
    assert_eq!(
        *ext.cursor.lock().expect("cursor"),
        Some(messages.last().expect("last").id),
        "the cursor re-seeds to the snapshot tail"
    );
}
