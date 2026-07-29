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

use houyicoder_context::{TurnEvent, TurnEventKind};

/// Extract the current turn's reasoning: scan from the last UserInput event
/// onward so a Ctrl+O expand shows only this turn's chain of thought, not a
/// concatenation of every prior turn's reasoning.
pub fn turn_reasoning(events: &[TurnEvent]) -> Option<String> {
    let last_user = events
        .iter()
        .rposition(|e| matches!(e.kind, TurnEventKind::UserInput { .. }));
    let start = last_user.map(|i| i + 1).unwrap_or(0);
    let mut r = String::new();
    for e in &events[start..] {
        if let TurnEventKind::Reasoning { text } = &e.kind {
            r.push_str(text);
        }
    }
    if r.is_empty() { None } else { Some(r) }
}

/// A one-line summary of the tools the current turn invoked, in the shape
/// the folded ThoughtFor row surfaces ("ran 3 tools (2 bash, 1 grep)").
/// Scans from the last UserInput onward so only this turn's tool calls land
/// in the summary. Returns None when the turn ran no tools.
pub fn turn_tool_summary(events: &[TurnEvent]) -> Option<String> {
    let last_user = events
        .iter()
        .rposition(|e| matches!(e.kind, TurnEventKind::UserInput { .. }));
    let start = last_user.map(|i| i + 1).unwrap_or(0);
    let mut counts: Vec<(String, u32)> = Vec::new();
    let mut total = 0u32;
    for e in &events[start..] {
        if let TurnEventKind::ToolCall { tool, .. } = &e.kind {
            if let Some(slot) = counts.iter_mut().find(|(t, _)| t == tool) {
                slot.1 += 1;
            } else {
                counts.push((tool.clone(), 1));
            }
            total += 1;
        }
    }
    if total == 0 {
        return None;
    }
    counts.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    let parts: Vec<String> = counts.iter().map(|(t, c)| format!("{c} {t}")).collect();
    let noun = if total == 1 { "tool" } else { "tools" };
    Some(format!("ran {total} {noun} ({})", parts.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::super::{RunOutcome, Runner, RunnerConfig, ToolRegistry};
    use super::*;
    use crate::provider::test_support::FakeProvider;
    use houyicoder_context::{SessionId, TurnEventKind};
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
    fn test_turn_reasoning_current_only() {
        // Ctrl+O expands only the current turn's reasoning, not a
        // concatenation of every prior turn's. The last UserInput marks the
        // turn boundary; reasoning before it must not leak into the expand.
        use houyicoder_context::{EventId, SessionId, TurnEvent};
        let ev = |kind| TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            kind,
        };
        let events = vec![
            ev(TurnEventKind::UserInput {
                text: "first".into(),
            }),
            ev(TurnEventKind::Reasoning {
                text: "first-thoughts".into(),
            }),
            ev(TurnEventKind::UserInput {
                text: "second".into(),
            }),
            ev(TurnEventKind::Reasoning {
                text: "second-thoughts".into(),
            }),
        ];
        let r = turn_reasoning(&events).expect("non-empty");
        assert!(r.contains("second-thoughts"), "current turn present: {r}");
        assert!(
            !r.contains("first-thoughts"),
            "prior turn reasoning must not leak: {r}"
        );
    }

    #[test]
    fn test_turn_tool_summary_current() {
        // The folded ThoughtFor row surfaces "ran N tools (...)" for the
        // current turn only (last UserInput onward), grouped + sorted by
        // count, so prior turns' tool calls do not leak.
        use houyicoder_context::{EventId, SessionId, TurnEvent};
        let ev = |kind| TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            kind,
        };
        let events = vec![
            ev(TurnEventKind::UserInput {
                text: "first".into(),
            }),
            ev(TurnEventKind::ToolCall {
                call_id: "c0".into(),
                tool: "bash".into(),
                input: serde_json::json!({}),
            }),
            ev(TurnEventKind::UserInput {
                text: "second".into(),
            }),
            ev(TurnEventKind::ToolCall {
                call_id: "c1".into(),
                tool: "grep".into(),
                input: serde_json::json!({}),
            }),
            ev(TurnEventKind::ToolCall {
                call_id: "c2".into(),
                tool: "bash".into(),
                input: serde_json::json!({}),
            }),
        ];
        let s = turn_tool_summary(&events).expect("non-empty");
        assert!(s.contains("ran 2 tools"), "{s}");
        assert!(s.contains("1 bash"), "{s}");
        assert!(s.contains("1 grep"), "{s}");
        assert!(!s.contains("first"), "prior turn must not leak: {s}");
    }

    #[test]
    fn test_turn_tool_summary_none() {
        use houyicoder_context::{EventId, SessionId, TurnEvent};
        let ev = |kind| TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            kind,
        };
        let events = vec![ev(TurnEventKind::UserInput { text: "x".into() })];
        assert!(
            turn_tool_summary(&events).is_none(),
            "no tools → no summary"
        );
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
        let msg = events.iter().find_map(|e| match &e.kind {
            TurnEventKind::AssistantMessage { text, thinking } => Some((text, thinking)),
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
            .filter(|e| matches!(e.kind, TurnEventKind::Reasoning { .. }))
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
        let msg = events.iter().find_map(|e| match &e.kind {
            TurnEventKind::AssistantMessage { thinking, .. } => thinking.clone(),
            _ => None,
        });
        assert!(msg.is_none(), "thinking must be None without reasoning");
    }
}
