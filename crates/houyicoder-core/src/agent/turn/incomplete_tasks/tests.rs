//! Unit tests for incomplete task detection and continuation nudges.

use super::*;
use houyicoder_context::{EventId, SessionId};
use serde_json::json;

fn make_entry(event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 1000,
        prev_hash: None,
        event,
    }
}

#[test]
fn test_incomplete_todos_detected() {
    let events = vec![
        make_entry(SessionEvent::UserInput {
            text: "Fix the bugs".into(),
        }),
        make_entry(SessionEvent::ToolCall {
            call_id: "c1".into(),
            tool: "todo_write".into(),
            input: json!({
                "todos": [
                    { "content": "task 1", "status": "completed" },
                    { "content": "task 2", "status": "pending" },
                ]
            }),
        }),
        make_entry(SessionEvent::ToolResult {
            call_id: "c1".into(),
            output: json!({
                "todos": [
                    { "content": "task 1", "status": "completed" },
                    { "content": "task 2", "status": "pending" },
                ],
                "pending": 1,
                "in_progress": 0,
                "completed": 1,
                "cleared": false,
            }),
            duration_ms: 5,
        }),
    ];

    let nudge = check_incomplete_tasks(&events, "I finished task 1. Shall I continue?");
    assert!(nudge.is_some());
    let msg = nudge.unwrap();
    assert!(msg.contains("incomplete item(s)"));
    assert!(msg.contains("task 2"));
}

#[test]
fn test_completed_todos_pass() {
    let events = vec![
        make_entry(SessionEvent::UserInput {
            text: "Fix the bugs".into(),
        }),
        make_entry(SessionEvent::ToolResult {
            call_id: "c1".into(),
            output: json!({
                "todos": [
                    { "content": "task 1", "status": "completed" },
                    { "content": "task 2", "status": "completed" },
                ],
                "pending": 0,
                "in_progress": 0,
                "completed": 2,
                "cleared": true,
            }),
            duration_ms: 5,
        }),
    ];

    let nudge = check_incomplete_tasks(&events, "All work is complete.");
    assert!(nudge.is_none());
}

#[test]
fn test_empty_todos_pass() {
    let events = vec![
        make_entry(SessionEvent::UserInput {
            text: "Fix the bugs".into(),
        }),
        make_entry(SessionEvent::ToolResult {
            call_id: "c1".into(),
            output: json!({
                "todos": [],
                "pending": 0,
                "in_progress": 0,
                "completed": 0,
                "cleared": true,
            }),
            duration_ms: 5,
        }),
    ];

    let nudge = check_incomplete_tasks(&events, "Everything done.");
    assert!(nudge.is_none());
}

#[test]
fn test_continuous_prompt_nudge() {
    let events = vec![make_entry(SessionEvent::UserInput {
        text: "请修复所有问题，不要停，一次性解决完".into(),
    })];

    let nudge = check_incomplete_tasks(&events, "已完成第一部分。要我接着做第二部分吗？");
    assert!(nudge.is_some());
    let msg = nudge.unwrap();
    assert!(msg.contains("end-to-end without stopping"));

    // Without asking for confirmation, do not intercept.
    let normal_nudge = check_incomplete_tasks(&events, "已经全部修复完成并通过测试。");
    assert!(normal_nudge.is_none());
}
