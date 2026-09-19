//! Brief helper for reasoning text: truncate to a one-line summary so a
//! collapsed thinking block fits in a single transcript row. The full text
//! stays in the AssistantMessage thinking field and the raw Reasoning events;
//! this helper only produces the label a host renders in the collapsed view.

/// Maximum characters in a brief before it is cut with an ellipsis.
const BRIEF_MAX: usize = 120;

/// Produce a one-line summary of reasoning text. Returns the first
/// non-empty line, truncated to BRIEF_MAX characters with an ellipsis when
/// it exceeds the budget. Returns an empty string for empty input.
pub fn thinking_brief(text: &str) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.len() <= BRIEF_MAX {
        return line.to_string();
    }
    let cut: String = line.chars().take(BRIEF_MAX).collect();
    let mut brief = cut;
    brief.push('\u{2026}');
    brief
}

#[cfg(test)]
mod tests {
    use super::super::{RunOutcome, Runner, RunnerConfig, ToolRegistry};
    use super::*;
    use crate::provider::test_support::FakeProvider;
    use houyicoder_context::{SessionEvent, SessionId};
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_protocol::llm::Usage;
    use houyicoder_protocol::llm::{CompletionResponse, OutputItem};
    use houyicoder_resilience::Retry;
    use houyicoder_session::SessionStore;
    use std::sync::Arc;

    fn runner(provider: Arc<dyn houyicoder_api::provider::ModelProvider>) -> Runner {
        Runner::new(
            std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
            provider,
            ToolRegistry::new(),
            RunnerConfig {
                model: "test".into(),
                instructions: "test agent".into(),
                max_turns: 5,
                max_output_tokens: 8_000,
                retry: Retry::default(),
            },
        )
    }

    #[test]
    fn test_brief_short_text_unchanged() {
        assert_eq!(thinking_brief("hello world"), "hello world");
    }

    #[test]
    fn test_brief_skips_blank_lines() {
        assert_eq!(thinking_brief("\n\n  \nactual thought"), "actual thought");
    }

    #[test]
    fn test_brief_truncates_long_text() {
        let long = "x".repeat(200);
        let brief = thinking_brief(&long);
        assert!(brief.ends_with('\u{2026}'));
        assert_eq!(brief.chars().count(), BRIEF_MAX + 1);
    }

    #[test]
    fn test_brief_empty_returns_empty() {
        assert_eq!(thinking_brief(""), "");
        assert_eq!(thinking_brief("\n\n  \n"), "");
    }

    #[test]
    fn test_brief_trims_whitespace() {
        assert_eq!(thinking_brief("  hello  "), "hello");
    }

    #[test]
    fn test_brief_multiline_first_nonempty() {
        let text = "line one\nline two\nline three";
        assert_eq!(thinking_brief(text), "line one");
    }

    #[tokio::test]
    async fn test_thinking_persists_with_reasoning() {
        let p = Arc::new(FakeProvider::new(vec![CompletionResponse {
            output: vec![
                OutputItem::Reasoning {
                    text: "step 1".into(),
                },
                OutputItem::Reasoning {
                    text: " step 2".into(),
                },
                OutputItem::Text {
                    text: "answer".into(),
                },
            ],
            usage: Usage::default(),
            model: "test".into(),
        }]));
        let runner = runner(p);
        let session = SessionId::new();
        let result = runner.run(session, "hi".into()).await.unwrap();
        assert!(matches!(result.outcome, RunOutcome::FinalOutput(t) if t == "answer"));
        let events = runner.store().replay(session).await.expect("replay");
        let msg = events.iter().find_map(|e| match &e.event {
            SessionEvent::AssistantMessage { text, thinking } => Some((text, thinking)),
            _ => None,
        });
        let (text, thinking) = msg.expect("AssistantMessage exists");
        assert_eq!(text, "answer");
        assert_eq!(
            thinking.as_ref().expect("thinking is Some"),
            "step 1 step 2"
        );
        let reasoning_n = events
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::Reasoning { .. }))
            .count();
        // The per-delta reasoning chunks join into ONE Reasoning event (one
        // thinking row in the transcript), not one per delta — a per-delta
        // event produced a word-chunked wall of thinking rows.
        assert_eq!(reasoning_n, 1, "joined Reasoning event persisted");
    }

    #[tokio::test]
    async fn test_thinking_none_without_reasoning() {
        let p = Arc::new(FakeProvider::text("plain answer"));
        let runner = runner(p);
        let session = SessionId::new();
        runner.run(session, "hi".into()).await.unwrap();
        let events = runner.store().replay(session).await.expect("replay");
        let msg = events.iter().find_map(|e| match &e.event {
            SessionEvent::AssistantMessage { thinking, .. } => thinking.clone(),
            _ => None,
        });
        assert!(msg.is_none(), "thinking must be None without reasoning");
    }
}
