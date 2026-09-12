//! Provider-backed summary generation for compaction.
//!
//! Input is batched at assistant-turn boundaries. Provider failures and empty
//! responses fall back to deterministic summarization.

use std::sync::Arc;

use houyicoder_api::provider::ModelProvider;
use houyicoder_async::PFut;
use houyicoder_context::{SessionEvent, SessionLogEntry};
use houyicoder_protocol::llm::{CompletionRequest, CompletionResponse, ModelSettings, OutputItem};

use super::super::manifest::{HeuristicSummarizer, SummarizeError, Summarizer};
use super::super::turn_group;

/// Structured instructions emphasizing the information needed to resume work.
const SUMMARIZER_INSTRUCTION: &str = "You are a helpful AI assistant tasked with \
summarizing the conversation below. Produce a plain-text summary with these \
sections, each a labeled paragraph (skip a section if it is empty):\n\
1. Primary intent: what the user asked for and the high-level goal of the session.\n\
2. Current work: the active task right before this summary — the last tool calls, \
the last result, and what was about to happen next.\n\
3. Pending: outstanding tasks, unanswered questions, and next steps the user expects.\n\
4. Key files and decisions: files read, changed, or written (concrete paths) and the \
important decisions or constraints established. Use paths and values, not prose.";

/// Default per-batch token budget for the LLM summarizer. The folded span is
/// split into batches so each fits well under the context window, preventing
/// the summarizer from itself triggering overflow.
const DEFAULT_BATCH_TOKEN_LIMIT: usize = 50_000;

/// An LLM-backed summarizer. Calls the provider with batched input (split by
/// assistant turns) to produce a summary of the folded span. Falls back to
/// the heuristic summarizer when the provider fails or returns no usable text.
pub struct LlmSummarizer {
    provider: Arc<dyn ModelProvider>,
    model: String,
    batch_token_limit: usize,
}

impl LlmSummarizer {
    /// Construct an LLM summarizer over the given provider. The model id is
    /// the one the main loop uses; batch_token_limit caps each summary batch.
    pub fn new(provider: Arc<dyn ModelProvider>, model: String) -> Self {
        Self {
            provider,
            model,
            batch_token_limit: DEFAULT_BATCH_TOKEN_LIMIT,
        }
    }
}

impl Summarizer for LlmSummarizer {
    fn summarize<'a>(
        &'a self,
        events: &'a [SessionLogEntry],
        custom_instructions: Option<&'a str>,
    ) -> PFut<'a, Result<String, SummarizeError>> {
        if events.is_empty() {
            return Box::pin(async move { Err(SummarizeError::Empty) });
        }

        let batches = build_summary_batches(events, self.batch_token_limit);
        let provider = self.provider.clone();
        let model = self.model.clone();

        Box::pin(async move {
            let instructions = match custom_instructions {
                Some(extra) if !extra.is_empty() => {
                    format!("{SUMMARIZER_INSTRUCTION}\n\n{extra}")
                }
                _ => SUMMARIZER_INSTRUCTION.to_string(),
            };
            let mut summaries: Vec<String> = Vec::with_capacity(batches.len());
            for batch in &batches {
                let input = turn_group::assemble_model_input(batch, None);
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
                            let fallback = HeuristicSummarizer.summarize(events, None).await;
                            return fallback.map_err(|_| {
                                SummarizeError::LlmFailed("no text in provider response".into())
                            });
                        }
                    },
                    Err(e) => {
                        let fallback = HeuristicSummarizer.summarize(events, None).await;
                        return fallback.map_err(|_| {
                            SummarizeError::LlmFailed(format!("provider error: {e}"))
                        });
                    }
                }
            }
            if summaries.is_empty() {
                return Err(SummarizeError::LlmFailed("no batches summarized".into()));
            }
            if summaries.len() == 1 {
                return summaries
                    .pop()
                    .ok_or_else(|| SummarizeError::LlmFailed("no batches summarized".into()));
            }
            Ok(summaries.join("\n\n---\n\n"))
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

/// Split events into summary batches, each staying under the token budget.
/// AssistantTextDelta is skipped (subsumed by the authoritative
/// AssistantMessage). A new batch starts at an AssistantMessage when the
/// current batch is non-empty and over budget.
fn build_summary_batches(
    events: &[SessionLogEntry],
    batch_token_limit: usize,
) -> Vec<Vec<SessionLogEntry>> {
    let byte_limit = batch_token_limit.saturating_mul(4);
    let mut batches: Vec<Vec<SessionLogEntry>> = Vec::new();
    let mut current: Vec<SessionLogEntry> = Vec::new();
    let mut current_bytes: usize = 0;

    for event in events {
        if matches!(event.event, SessionEvent::AssistantTextDelta { .. }) {
            continue;
        }
        let event_bytes = event_byte_len(event);

        if matches!(event.event, SessionEvent::AssistantMessage { .. })
            && !current.is_empty()
            && current_bytes > byte_limit
        {
            batches.push(std::mem::take(&mut current));
            current_bytes = 0;
        }

        current.push(event.clone());
        current_bytes += event_bytes;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// Rough byte length of an event text content (for batch budgeting).
fn event_byte_len(event: &SessionLogEntry) -> usize {
    match &event.event {
        SessionEvent::UserInput { text }
        | SessionEvent::MetaUser { text }
        | SessionEvent::MidTurnInput { text, .. }
        | SessionEvent::MemoryRecall { text, .. }
        | SessionEvent::SkillListing { text, .. } => text.len(),
        SessionEvent::SkillBody { content, .. } => content.len(),
        SessionEvent::RewardObservation { .. } => 0,
        SessionEvent::Unknown => 0,
        SessionEvent::AssistantMessage { text, thinking } => {
            text.len() + thinking.as_ref().map(String::len).unwrap_or(0)
        }
        SessionEvent::AssistantTextDelta { .. } => 0,
        SessionEvent::ToolCall { input, .. } => input.to_string().len(),
        SessionEvent::ToolResult { output, .. } => output.to_string().len(),
        SessionEvent::Reasoning { text } => text.len(),
        SessionEvent::CompactionBoundary { .. } => 0,
        SessionEvent::CacheBreak { .. } => 0,
        SessionEvent::Summary { text } => text.len(),
        SessionEvent::PermissionDecision { .. } => 0,
        SessionEvent::TurnAborted { reason } => reason.len(),
        SessionEvent::TruncationVerdict { .. } => 0,
        SessionEvent::WorktreeEnter { .. } | SessionEvent::WorktreeExit { .. } => 0,
        SessionEvent::TurnUsage { .. }
        | SessionEvent::HookSignal { .. }
        | SessionEvent::TurnStarted { .. }
        | SessionEvent::SubagentSpawn { .. }
        | SessionEvent::SubagentReturn { .. }
        | SessionEvent::NotificationInjected { .. } => 0,
    }
}

#[cfg(test)]
mod tests {
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
}
