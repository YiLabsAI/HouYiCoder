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

/// The item kinds in order, so a test can state the projected shape.
fn shape(items: &[InputItem]) -> Vec<&'static str> {
    items
        .iter()
        .map(|item| match item {
            InputItem::User { .. } => "user",
            InputItem::Assistant { .. } => "assistant",
            InputItem::ToolResult { .. } => "tool",
        })
        .collect()
}

fn recall(text: &str) -> SessionEvent {
    SessionEvent::MemoryRecall {
        text: text.into(),
        keys: Vec::new(),
        bytes: 0,
    }
}

fn call(call_id: &str) -> SessionEvent {
    SessionEvent::ToolCall {
        call_id: call_id.into(),
        tool: "bash".into(),
        input: serde_json::json!({}),
    }
}

fn reply(text: &str) -> SessionEvent {
    SessionEvent::AssistantMessage {
        text: text.into(),
        thinking: None,
    }
}

/// A recall that lands while tool results are outstanding waits for them.
///
/// A provider rejects an assistant tool-call message that is not followed by
/// one tool message per call, so appending the recall in log order would make
/// the request invalid. The recall is held until the calls are answered, and
/// the log itself keeps its original order.
#[test]
fn test_recall_waits_for_tools() {
    let events = vec![
        evt(user("check the tree")),
        evt(reply("")),
        evt(call("c1")),
        evt(call("c2")),
        evt(recall("first recall")),
        evt(recall("second recall")),
        evt(SessionEvent::tool_result("c1", serde_json::json!({"a": 1}))),
        evt(SessionEvent::tool_result("c2", serde_json::json!({"b": 2}))),
    ];
    let items = assemble_model_input(&events, None);
    assert_eq!(
        shape(&items),
        vec!["user", "assistant", "tool", "tool", "user"],
        "the recalls follow both results: {items:?}"
    );
    match &items[4] {
        InputItem::User { content } => {
            assert!(content.contains("first recall"), "first held: {content}");
            assert!(content.contains("second recall"), "second held: {content}");
            let first = content.find("first recall").expect("first present");
            let second = content.find("second recall").expect("second present");
            assert!(first < second, "held text keeps its order: {content}");
        }
        other => panic!("expected the held recalls as a user item, got {other:?}"),
    }
}

/// A single result does not release the text while another call is open; at
/// window end the unanswered second call gets a stub tool result before the
/// held text is released, keeping the message sequence valid.
#[test]
fn test_recall_waits_all_calls() {
    let events = vec![
        evt(user("go")),
        evt(reply("")),
        evt(call("c1")),
        evt(call("c2")),
        evt(recall("held")),
        evt(SessionEvent::tool_result("c1", serde_json::json!({"a": 1}))),
    ];
    let items = assemble_model_input(&events, None);
    assert_eq!(
        shape(&items),
        vec!["user", "assistant", "tool", "tool", "user"],
        "both calls are answered before the held text: {items:?}"
    );
}

/// A call an aborted turn never answered gets a stub tool result and releases
/// the held text before the next reply, where the user wrote it.
#[test]
fn test_held_before_reply() {
    let events = vec![
        evt(user("first")),
        evt(reply("")),
        evt(call("c1")),
        evt(user("second")),
        evt(reply("answer")),
    ];
    let items = assemble_model_input(&events, None);
    assert_eq!(
        shape(&items),
        vec!["user", "assistant", "tool", "user", "assistant"],
        "the unanswered call is closed before the held user message: {items:?}"
    );
    match &items[3] {
        InputItem::User { content } => assert_eq!(content, "second"),
        other => panic!("expected the held message, got {other:?}"),
    }
}

/// Text held to the end of the window is appended after closing any unanswered
/// tool call, never dropped.
#[test]
fn test_held_at_window_end() {
    let events = vec![
        evt(user("go")),
        evt(reply("")),
        evt(call("c1")),
        evt(recall("late recall")),
    ];
    let items = assemble_model_input(&events, None);
    assert_eq!(shape(&items), vec!["user", "assistant", "tool", "user"]);
    match &items[3] {
        InputItem::User { content } => assert!(content.contains("late recall")),
        other => panic!("expected the held recall, got {other:?}"),
    }
}

/// A recall that lands between two calls of one assistant message joins them
/// back into one message. The calls are appended one at a time, so this gap is
/// reachable; splitting them would leave the first call unanswered by a tool
/// message, which is the same rejection the deferral exists to prevent.
#[test]
fn test_recall_between_calls() {
    let events = vec![
        evt(user("go")),
        evt(reply("")),
        evt(call("c1")),
        evt(recall("between")),
        evt(call("c2")),
        evt(SessionEvent::tool_result("c1", serde_json::json!({"a": 1}))),
        evt(SessionEvent::tool_result("c2", serde_json::json!({"b": 2}))),
    ];
    let items = assemble_model_input(&events, None);
    assert_eq!(
        shape(&items),
        vec!["user", "assistant", "tool", "tool", "user"],
        "one assistant message carries both calls: {items:?}"
    );
    match &items[1] {
        InputItem::Assistant { tool_calls, .. } => {
            let ids: Vec<&str> = tool_calls.iter().map(|call| call.id.as_str()).collect();
            assert_eq!(ids, vec!["c1", "c2"], "both calls ride one message");
        }
        other => panic!("expected one assistant item, got {other:?}"),
    }
}

/// A child-completion notice is held like any other user-role text: it is not
/// the model's to read before the call it interrupted is answered.
#[test]
fn test_notice_waits_for_tools() {
    let events = vec![
        evt(user("go")),
        evt(reply("")),
        evt(call("c1")),
        evt(SessionEvent::NotificationInjected {
            child_session_id: "child".into(),
            turn: 1,
            order: 0,
            topic: "done".into(),
            summary: "child finished".into(),
        }),
        evt(SessionEvent::tool_result("c1", serde_json::json!({"a": 1}))),
    ];
    let items = assemble_model_input(&events, None);
    assert_eq!(
        shape(&items),
        vec!["user", "assistant", "tool", "user"],
        "the notice follows the result: {items:?}"
    );
    match &items[3] {
        InputItem::User { content } => {
            assert!(
                content.contains("child finished"),
                "notice present: {content}"
            );
        }
        other => panic!("expected the held notice, got {other:?}"),
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
