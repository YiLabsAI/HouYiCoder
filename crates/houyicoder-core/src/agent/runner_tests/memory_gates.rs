//! Gate tests for the memory-recall path: the cumulative byte cap and the
//! auto_memory toggle. Call memory.recall directly (no run) so the
//! assembled context is never tokenized — the gates read byte counts and a bool,
//! not tokens, and the tests stay sub-millisecond under any tokenizer.

use super::*;
use crate::agent::MemoryRuntime;
use crate::provider::test_support::FakeProvider;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::stream_from_response;
use houyicoder_context::EventId;
use houyicoder_context::{MemoryEntry, MemorySource, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_memory::KeywordRecallProvider;
use houyicoder_memory::MarkdownMemoryProvider;
use houyicoder_protocol::llm::CompletionRequest;
use houyicoder_protocol::llm::{CompletionResponse, ModelCapabilities, OutputItem, ProviderError};
use houyicoder_protocol::llm::{LlmEvent, Usage};
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::fs;
use std::fs::remove_dir_all;
use std::process::id;
use std::sync::Arc;

fn runner_with(provider: Arc<dyn ModelProvider>, tools: ToolRegistry) -> Runner {
    Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            max_output_tokens: 8_000,
            retry: Retry {
                max_attempts: 2,
                ..Retry::default()
            },
        },
    )
}

/// A provider stub used by the gate tests (the run path is not exercised;
/// memory.recall is called directly, so this only needs to satisfy the
/// constructor).
struct StubGateProvider;

impl ModelProvider for StubGateProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> houyicoder_async::PFut<'_, Result<CompletionResponse, ProviderError>> {
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
    fn stream(
        &self,
        _req: CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
        stream_from_response(CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        })
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// The cumulative byte cap skips recall once the session has surfaced enough
/// memory. A pre-populated memory-recall event over the 60KB cap means the
/// next inject does not call recall, so no new memory-recall event is
/// appended. Calls memory.recall directly (no run) — the gate reads
/// byte counts, not tokens.
#[tokio::test]
async fn test_byte_cap_skips_recall() {
    let provider: Arc<dyn ModelProvider> = Arc::new(StubGateProvider);
    let memory: Arc<dyn MemoryProvider> =
        Arc::new(KeywordRecallProvider::with_entries(vec![MemoryEntry::new(
            "fact-key",
            "matching fact content here",
            MemorySource::Project,
        )]));
    let mut runner = runner_with(provider, ToolRegistry::new());
    runner.memory.install_provider(memory);
    let session = SessionId::new();
    // A user input that matches the seeded entry, plus a memory-recall
    // event over the 60KB cap. The byte-cap gate fires before the query
    // reaches the provider, so recall is skipped.
    for kind in [
        SessionEvent::UserInput {
            text: "matching fact query".into(),
        },
        SessionEvent::MemoryRecall {
            text: "x".repeat(60 * 1024 + 100),
            keys: vec!["saturated".into()],
            bytes: (60 * 1024 + 100) as u32,
        },
    ] {
        runner
            .store()
            .append(SessionLogEntry {
                id: EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                event: kind,
            })
            .await
            .unwrap();
    }
    runner.memory.recall(session).await.unwrap();
    let events = runner.store().replay(session).await.unwrap();
    let recall_events = events
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::MemoryRecall { .. }))
        .count();
    assert_eq!(
        recall_events, 1,
        "byte cap must skip recall when the session is saturated"
    );
}

/// A successful recall (auto_memory on, multi-word matching query, under the
/// byte cap) appends a MemoryRecall event whose durable bytes field equals
/// the rendered recall text length — the self-evolution loop reads this
/// cost dimension from the log without re-measuring. Covers the emit path.
#[tokio::test]
async fn test_recall_records_bytes() {
    let provider: Arc<dyn ModelProvider> = Arc::new(StubGateProvider);
    let memory: Arc<dyn MemoryProvider> =
        Arc::new(KeywordRecallProvider::with_entries(vec![MemoryEntry::new(
            "fact-key",
            "matching fact content here",
            MemorySource::Project,
        )]));
    let mut runner = runner_with(provider, ToolRegistry::new());
    runner.memory.install_provider(memory);
    let session = SessionId::new();
    runner
        .store()
        .append(SessionLogEntry {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "matching fact query".into(),
            },
        })
        .await
        .unwrap();
    runner.memory.recall(session).await.unwrap();
    let events = runner.store().replay(session).await.unwrap();
    let recalled = events
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::MemoryRecall { text, bytes, .. } => Some((text.clone(), *bytes)),
            _ => None,
        })
        .expect("a matching query appends a memory-recall event");
    assert_eq!(
        recalled.1 as usize,
        recalled.0.len(),
        "bytes field == rendered recall text length"
    );
    assert!(recalled.1 > 0, "the recall payload is non-empty");
}

/// auto_memory off skips turn-entry recall even when the store has a matching
/// entry + the query is multi-word: the toggle gate is the first check, before
/// the surfaced/byte-cap/query gates. Covers the toggle gate line.
#[tokio::test]
async fn test_toggle_off_skips_recall() {
    let provider: Arc<dyn ModelProvider> = Arc::new(StubGateProvider);
    let memory: Arc<dyn MemoryProvider> =
        Arc::new(KeywordRecallProvider::with_entries(vec![MemoryEntry::new(
            "fact-key",
            "matching fact content here",
            MemorySource::Project,
        )]));
    let mut runner = runner_with(provider, ToolRegistry::new());
    runner.memory.set_auto_memory(false);
    runner.memory.install_provider(memory);
    let session = SessionId::new();
    runner
        .store()
        .append(SessionLogEntry {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "matching fact query".into(),
            },
        })
        .await
        .unwrap();
    runner.memory.recall(session).await.unwrap();
    let events = runner.store().replay(session).await.unwrap();
    assert!(
        events
            .iter()
            .all(|e| !matches!(e.event, SessionEvent::MemoryRecall { .. })),
        "auto_memory off must skip recall entirely"
    );
}

/// The frozen index obeys the same auto-memory gate as recall: a disabled
/// gate injects no index even with entries on disk, and re-enabling rebuilds
/// from the store instead of replaying a disabled-era snapshot.
#[test]
fn test_memory_index_respects_gate() {
    let root = temp_dir().join(format!("mem-gate-{}-{}", id(), line!()));
    drop(fs::remove_dir_all(&root));
    fs::create_dir_all(&root).expect("mkdir");
    let memory: Arc<dyn MemoryProvider> = Arc::new(MarkdownMemoryProvider::new(root.clone()));
    memory
        .add(MemoryEntry::new(
            "gated-key",
            "carries a description",
            MemorySource::Project,
        ))
        .unwrap();
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let mut runtime = MemoryRuntime::new(store.clone());
    runtime.install_provider(memory.clone());
    let runner = Runner::new(
        store,
        Arc::new(FakeProvider::new(vec![])),
        ToolRegistry::new(),
        RunnerConfig::default(),
    )
    .install_memory(runtime);
    runner.set_auto_memory(false);
    assert_eq!(
        runner.format_memory_index(),
        None,
        "a disabled gate injects no index even with entries on disk"
    );
    runner.set_auto_memory(true);
    let reopened = runner.format_memory_index().expect("gate on rebuilds");
    assert!(
        reopened.contains("gated-key"),
        "re-enabling rebuilds rather than replaying a disabled-era cache: {reopened}"
    );
    drop(remove_dir_all(&root));
}
