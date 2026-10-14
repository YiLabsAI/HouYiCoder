//! Tests for this module.

use super::*;
use crate::provider::test_support::FakeProvider;
use houyicoder_context::{EventId, SessionId};
use houyicoder_protocol::llm::{ProviderError, Usage};

fn ev(session: SessionId, id: EventId, kind: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id,
        session,
        ts: 0,
        prev_hash: None,
        event: kind,
    }
}

fn user(text: &str) -> SessionEvent {
    SessionEvent::UserInput { text: text.into() }
}

fn assistant(text: &str) -> SessionEvent {
    SessionEvent::AssistantMessage {
        text: text.into(),
        thinking: None,
    }
}

/// An Unknown kind (a future-binary event type the current binary does not
/// recognize) carries no byte length — it budgets as zero so the batch
/// estimator does not choke on a forward-compatible event.
#[test]
fn test_byte_len_unknown_zero() {
    let e = ev(SessionId::new(), EventId::new(), SessionEvent::Unknown);
    assert_eq!(event_byte_len(&e), 0);
}

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

#[tokio::test]
async fn test_llm_summarizer_produces_summary() {
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
        ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
            Box::pin(iter(Vec::new()))
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
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
/// extra; the provider receives the merged string.
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

/// Empty custom instructions fall to the default instruction branch.
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
/// heuristic fallback.
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

#[test]
fn test_byte_len_spawn_zero() {
    let ev = SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: SessionEvent::SubagentSpawn {
            child_session_id: "c".into(),
            subagent_type: "explore".into(),
            prompt_summary: "find auth".into(),
            isolation: "worktree".into(),
            policy: "delegate".into(),
            trigger_source: "model:call-1".into(),
        },
    };
    assert_eq!(event_byte_len(&ev), 0);
}
