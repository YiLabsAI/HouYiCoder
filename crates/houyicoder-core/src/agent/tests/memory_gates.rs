//! Gate tests for the memory-recall path: the cumulative byte cap and the
//! auto_memory toggle. Call inject_memory_recall directly (no run) so the
//! served view is never tokenized — the gates read byte counts and a bool,
//! not tokens, and the tests stay sub-millisecond under any tokenizer.

use super::*;
use houyicoder_context::{MemoryEntry, MemorySource, SessionId, TurnEvent, TurnEventKind};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{CompletionResponse, ModelCapabilities, OutputItem, ProviderError};
use houyicoder_protocol::llm::{LlmEvent, Usage};
use houyicoder_resilience::Retry;
use houyicoder_session::SessionStore;

fn runner_with(provider: Arc<dyn ModelProvider>, tools: ToolRegistry) -> Runner {
    Runner::new(
        std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
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
/// inject_memory_recall is called directly, so this only needs to satisfy the
/// constructor).
struct StubGateProvider;

impl ModelProvider for StubGateProvider {
    fn complete(
        &self,
        _req: houyicoder_protocol::llm::CompletionRequest,
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
        _req: houyicoder_protocol::llm::CompletionRequest,
    ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
        houyicoder_api::provider::stream_from_response(CompletionResponse {
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
/// appended. Calls inject_memory_recall directly (no run) so the served
/// view is never tokenized — the gate reads byte counts, not tokens.
#[tokio::test]
async fn test_byte_cap_skips_recall() {
    let provider: Arc<dyn ModelProvider> = Arc::new(StubGateProvider);
    let memory: Arc<dyn houyicoder_api::memory::MemoryProvider> = Arc::new(
        houyicoder_memory::KeywordRecallProvider::with_entries(vec![MemoryEntry::new(
            "fact-key",
            "matching fact content here",
            MemorySource::Project,
        )]),
    );
    let runner = runner_with(provider, ToolRegistry::new()).with_memory(memory);
    let session = SessionId::new();
    // A multi-word user input (so the single-word gate would NOT skip) plus
    // a memory-recall event over the 60KB cap. The byte-cap gate fires
    // before the query/single-word gate, so recall is skipped.
    for kind in [
        TurnEventKind::UserInput {
            text: "matching fact query".into(),
        },
        TurnEventKind::MemoryRecall {
            text: "x".repeat(60 * 1024 + 100),
            keys: vec!["saturated".into()],
            bytes: (60 * 1024 + 100) as u32,
        },
    ] {
        runner
            .store()
            .append(TurnEvent {
                id: houyicoder_context::EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                kind,
            })
            .await
            .unwrap();
    }
    runner.inject_memory_recall(session).await.unwrap();
    let events = runner.store().replay(session).await.unwrap();
    let recall_events = events
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::MemoryRecall { .. }))
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
    let memory: Arc<dyn houyicoder_api::memory::MemoryProvider> = Arc::new(
        houyicoder_memory::KeywordRecallProvider::with_entries(vec![MemoryEntry::new(
            "fact-key",
            "matching fact content here",
            MemorySource::Project,
        )]),
    );
    let runner = runner_with(provider, ToolRegistry::new()).with_memory(memory);
    let session = SessionId::new();
    runner
        .store()
        .append(TurnEvent {
            id: houyicoder_context::EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::UserInput {
                text: "matching fact query".into(),
            },
        })
        .await
        .unwrap();
    runner.inject_memory_recall(session).await.unwrap();
    let events = runner.store().replay(session).await.unwrap();
    let recalled = events
        .iter()
        .find_map(|e| match &e.kind {
            TurnEventKind::MemoryRecall { text, bytes, .. } => Some((text.clone(), *bytes)),
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
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    let provider: Arc<dyn ModelProvider> = Arc::new(StubGateProvider);
    let memory: Arc<dyn houyicoder_api::memory::MemoryProvider> = Arc::new(
        houyicoder_memory::KeywordRecallProvider::with_entries(vec![MemoryEntry::new(
            "fact-key",
            "matching fact content here",
            MemorySource::Project,
        )]),
    );
    let auto_memory = Arc::new(AtomicBool::new(false));
    let auto_dream = Arc::new(AtomicBool::new(true));
    let runner = runner_with(provider, ToolRegistry::new())
        .with_memory(memory)
        .with_toggles(auto_memory, auto_dream);
    let session = SessionId::new();
    runner
        .store()
        .append(TurnEvent {
            id: houyicoder_context::EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::UserInput {
                text: "matching fact query".into(),
            },
        })
        .await
        .unwrap();
    runner.inject_memory_recall(session).await.unwrap();
    let events = runner.store().replay(session).await.unwrap();
    assert!(
        events
            .iter()
            .all(|e| !matches!(e.kind, TurnEventKind::MemoryRecall { .. })),
        "auto_memory off must skip recall entirely"
    );
}
