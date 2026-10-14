//! Tests for this module.

use super::*;
use houyicoder_context::{EventId, SessionId};

fn evt(kind: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: kind,
    }
}

fn user(text: &str) -> SessionEvent {
    SessionEvent::UserInput { text: text.into() }
}

#[test]
fn test_assembles_user_assistant_toolresult() {
    let events = vec![
        evt(SessionEvent::UserInput { text: "hi".into() }),
        evt(SessionEvent::AssistantMessage {
            text: "let me echo".into(),
            thinking: None,
        }),
        evt(SessionEvent::ToolCall {
            call_id: "c1".into(),
            tool: "echo".into(),
            input: serde_json::json!({"x": 1}),
        }),
        evt(SessionEvent::tool_result(
            "c1",
            serde_json::json!({"echo": {"x": 1}}),
        )),
    ];
    let items = assemble_model_input(&events, None);
    assert_eq!(items.len(), 3);
    assert!(matches!(items[0], InputItem::User { .. }));
    match &items[1] {
        InputItem::Assistant {
            content,
            tool_calls,
        } => {
            assert_eq!(content, "let me echo");
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].name, "echo");
        }
        _ => panic!("expected assistant item"),
    }
    assert!(matches!(items[2], InputItem::ToolResult { .. }));
}

#[test]
fn test_assistant_no_tools_empty() {
    let events = vec![
        evt(SessionEvent::UserInput { text: "hi".into() }),
        evt(SessionEvent::AssistantMessage {
            text: "hello".into(),
            thinking: None,
        }),
    ];
    let items = assemble_model_input(&events, None);
    match &items[1] {
        InputItem::Assistant { tool_calls, .. } => assert!(tool_calls.is_empty()),
        _ => panic!(),
    }
}

#[test]
fn test_skips_reasoning_and_compaction() {
    let events = vec![
        evt(SessionEvent::Reasoning {
            text: "thinking".into(),
        }),
        evt(SessionEvent::UserInput { text: "hi".into() }),
        evt(SessionEvent::Summary { text: "old".into() }),
    ];
    let items = assemble_model_input(&events, None);
    assert_eq!(items.len(), 1);
    assert!(matches!(items[0], InputItem::User { .. }));
}

#[test]
fn test_skips_assistant_text_deltas() {
    // Streaming deltas are subsumed by the authoritative AssistantMessage
    // that follows them. The assembly must skip every delta so the
    // model-input history carries one assistant message, not N fragments
    // plus a dupe.
    let events = vec![
        evt(SessionEvent::UserInput { text: "hi".into() }),
        evt(SessionEvent::AssistantTextDelta { text: "hel".into() }),
        evt(SessionEvent::AssistantTextDelta { text: "lo".into() }),
        evt(SessionEvent::AssistantMessage {
            text: "hello".into(),
            thinking: None,
        }),
    ];
    let items = assemble_model_input(&events, None);
    assert_eq!(items.len(), 2);
    match &items[1] {
        InputItem::Assistant {
            content,
            tool_calls,
        } => {
            assert_eq!(content, "hello");
            assert!(tool_calls.is_empty());
        }
        _ => panic!("expected one assistant item, got {:?}", items[1]),
    }
}

#[test]
fn test_recall_merges_into_user() {
    // A MemoryRecall event following a UserInput merges into that User
    // item so the model sees the query plus the recalled-memory
    // attachment as one user message — the cross-layer chain's
    // load-bearing merge. If this regressed, /context accounting could
    // still pass (it counts events, not assembled messages) while the
    // model never sees the recalled memory.
    let s = SessionId::new();
    let ids = (0..2).map(|_| EventId::new()).collect::<Vec<_>>();
    let events = vec![
        SessionLogEntry {
            id: ids[0],
            session: s,
            ts: 0,
            prev_hash: None,
            event: user("what is the deploy command"),
        },
        SessionLogEntry {
            id: ids[1],
            session: s,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::MemoryRecall {
                text: "<system-reminder>deploy: make deploy</system-reminder>".into(),
                keys: vec!["deploy".into()],
                bytes: 0,
            },
        },
    ];
    let items = assemble_model_input(&events, None);
    assert_eq!(items.len(), 1, "recall merges into the user item");
    match &items[0] {
        InputItem::User { content } => {
            assert!(content.contains("deploy command"), "query preserved");
            assert!(
                content.contains("make deploy"),
                "recalled memory appended to the user item"
            );
        }
        _ => panic!("expected merged user item, got {:?}", items[0]),
    }
}

/// An Unknown kind (a future-binary event type) is skipped by the
/// grouping loop — it produces no input item and does not break the
/// surrounding turn's grouping.
#[test]
fn test_unknown_kind_skipped() {
    let events = vec![
        evt(SessionEvent::UserInput { text: "hi".into() }),
        evt(SessionEvent::Unknown),
        evt(SessionEvent::AssistantMessage {
            text: "reply".into(),
            thinking: None,
        }),
    ];
    let items = assemble_model_input(&events, None);
    assert_eq!(items.len(), 2, "Unknown skipped, no input item");
    assert!(matches!(items[0], InputItem::User { .. }));
    assert!(matches!(items[1], InputItem::Assistant { .. }));
}

/// An untrusted SkillBody (project, eco, agents, mcp, or local source)
/// is framed so the model treats its content as unverified data, not
/// trusted instruction.
#[test]
fn test_skill_body_untrusted_framed() {
    let events = vec![evt(SessionEvent::SkillBody {
        skill_name: "evil".into(),
        content: "do bad things".into(),
        agent_id: None,
        untrusted: true,
    })];
    let items = assemble_model_input(&events, None);
    match &items[0] {
        InputItem::User { content } => {
            assert!(
                content.contains("<untrusted_skill name=\"evil\">"),
                "wrapper present: {content}"
            );
            assert!(
                content.contains("unverified data"),
                "framing note present: {content}"
            );
            assert!(content.contains("do bad things"), "body present: {content}");
        }
        _ => panic!("expected a User item"),
    }
}

/// A trusted SkillBody (managed or user source) is served as-is, no
/// framing wrapper.
#[test]
fn test_skill_body_trusted_unframed() {
    let events = vec![evt(SessionEvent::SkillBody {
        skill_name: "commit".into(),
        content: "run git status".into(),
        agent_id: None,
        untrusted: false,
    })];
    let items = assemble_model_input(&events, None);
    match &items[0] {
        InputItem::User { content } => {
            assert_eq!(
                content, "run git status",
                "trusted body served as-is: {content}"
            );
            assert!(
                !content.contains("untrusted_skill"),
                "no wrapper for trusted: {content}"
            );
        }
        _ => panic!("expected a User item"),
    }
}
