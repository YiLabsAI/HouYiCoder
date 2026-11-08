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

    let nudge = check_incomplete_tasks(&events);
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

    assert!(check_incomplete_tasks(&events).is_none());
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

    assert!(check_incomplete_tasks(&events).is_none());
}

#[test]
fn test_stale_prior_turn_ignored() {
    let events = vec![
        make_entry(SessionEvent::UserInput {
            text: "Old multi-step task".into(),
        }),
        make_entry(SessionEvent::ToolResult {
            call_id: "c1".into(),
            output: json!({
                "todos": [
                    { "content": "old pending task", "status": "pending" },
                ],
                "pending": 1,
                "in_progress": 0,
                "completed": 0,
                "cleared": false,
            }),
            duration_ms: 5,
        }),
        make_entry(SessionEvent::UserInput {
            text: "What branch am I on?".into(),
        }),
    ];

    assert!(check_incomplete_tasks(&events).is_none());
}

#[test]
fn test_background_task_waits() {
    let mut events = vec![
        make_entry(SessionEvent::UserInput {
            text: "Run suite and fix".into(),
        }),
        make_entry(SessionEvent::ToolResult {
            call_id: "c1".into(),
            output: json!({
                "todos": [
                    { "content": "run suite", "status": "in_progress" },
                ],
                "pending": 0,
                "in_progress": 1,
                "completed": 0,
                "cleared": false,
            }),
            duration_ms: 5,
        }),
        make_entry(SessionEvent::ToolResult {
            call_id: "c2".into(),
            output: json!({
                "stdout": "",
                "stderr": "moved to the background as task #1",
                "background_task": 1,
            }),
            duration_ms: 15_000,
        }),
    ];

    assert!(check_incomplete_tasks(&events).is_none());

    events.push(make_entry(SessionEvent::NotificationInjected {
        child_session_id: "task-1".into(),
        turn: 1,
        order: 0,
        topic: "task".into(),
        summary: "task #1 finished".into(),
        capture: None,
    }));

    assert!(check_incomplete_tasks(&events).is_some());
}

#[test]
fn test_mid_turn_resets_cap() {
    let mut events = vec![
        make_entry(SessionEvent::UserInput {
            text: "Do work".into(),
        }),
        make_entry(SessionEvent::ToolResult {
            call_id: "c1".into(),
            output: json!({
                "todos": [
                    { "content": "task 1", "status": "pending" },
                ],
                "pending": 1,
                "in_progress": 0,
                "completed": 0,
                "cleared": false,
            }),
            duration_ms: 5,
        }),
    ];

    for _ in 0..MAX_TODO_CONTINUATIONS {
        let nudge = check_incomplete_tasks(&events).expect("nudge before cap");
        events.push(make_entry(SessionEvent::MetaUser { text: nudge }));
    }
    assert!(check_incomplete_tasks(&events).is_none());

    events.push(make_entry(SessionEvent::MidTurnInput {
        text: "keep going with the new parameters".into(),
        pending_input_id: None,
    }));
    assert!(check_incomplete_tasks(&events).is_some());
}
