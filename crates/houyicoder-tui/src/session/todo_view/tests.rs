//! The checklist accumulator: how a todo_write frame parses, and what the
//! state does across a live run, a replay, and a rewind.

use super::*;
use std::slice::from_ref;

use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{
    ContentChunk, SessionUpdate, ToolCall, ToolCallId,
};

fn todo_write_frame(todos_json: serde_json::Value) -> SessionUpdate {
    let mut tc = ToolCall::new(ToolCallId::new("c1"), "todo_write");
    tc.raw_input = Some(todos_json);
    SessionUpdate::ToolCall(tc)
}

fn non_todo_frame() -> SessionUpdate {
    SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text { text: "hi".into() }))
}

#[test]
fn test_from_tool_parses_items() {
    let payload = serde_json::json!({
        "todos": [
            { "content": "run tests", "status": "in_progress", "activeForm": "running tests" },
            { "content": "write docs", "status": "pending" },
            { "content": "ship", "status": "completed" }
        ]
    });
    let views = from_tool_call(&todo_write_frame(payload)).expect("todo-write frame");
    assert_eq!(views.len(), 3);
    assert_eq!(views[0].status, TodoStatus::InProgress);
    assert_eq!(views[0].active_form.as_deref(), Some("running tests"));
    assert_eq!(views[1].status, TodoStatus::Pending);
    assert!(views[1].active_form.is_none());
    assert_eq!(views[2].status, TodoStatus::Completed);
}

#[test]
fn test_ignores_non_todo_calls() {
    assert!(from_tool_call(&non_todo_frame()).is_none());
}

#[test]
fn test_missing_payload_ignored() {
    let mut tc = ToolCall::new(ToolCallId::new("c1"), "todo_write");
    tc.raw_input = None;
    assert!(from_tool_call(&SessionUpdate::ToolCall(tc)).is_none());
}

#[test]
fn test_empty_payload_clears() {
    let views = from_tool_call(&todo_write_frame(serde_json::json!({ "todos": [] })))
        .expect("valid empty update");
    assert!(views.is_empty());
}

#[test]
fn test_from_snake_maps_unknown() {
    assert_eq!(TodoStatus::from_snake("pending"), TodoStatus::Pending);
    assert_eq!(
        TodoStatus::from_snake("in_progress"),
        TodoStatus::InProgress
    );
    assert_eq!(TodoStatus::from_snake("completed"), TodoStatus::Completed);
    assert_eq!(TodoStatus::from_snake("bogus"), TodoStatus::Pending);
}

#[test]
fn test_run_end_pauses_existing() {
    let frame = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
        "todos": [{"content": "unfinished", "status": "in_progress"}]
    })));
    let mut state = TodoState::default();
    state.update(from_ref(&frame), 0, true);
    assert_eq!(state.items[0].status, TodoStatus::InProgress);

    state.update(from_ref(&frame), 0, false);
    assert_eq!(state.items[0].status, TodoStatus::Paused);
}

#[test]
fn test_prune_hides_done() {
    let now = Instant::now();
    let mut state = TodoState {
        items: vec![TodoView {
            content: "done".into(),
            status: TodoStatus::Completed,
            active_form: None,
        }],
        completion_at: HashMap::from([("done".to_string(), now - Duration::from_secs(6))]),
        ..Default::default()
    };

    assert!(state.prune(now));
    assert!(state.items.is_empty());
    assert!(state.completion_at.is_empty());
}

#[test]
fn test_prune_keeps_open() {
    let now = Instant::now();
    let mut state = TodoState {
        items: vec![TodoView {
            content: "open".into(),
            status: TodoStatus::Pending,
            active_form: None,
        }],
        completion_at: HashMap::new(),
        ..Default::default()
    };

    assert!(!state.prune(now));
    assert_eq!(state.items.len(), 1);
}

/// A cold replay of an all-completed list clears on the spot: the view
/// installs nothing and records no timestamps, and every fresh
/// accumulator (each resume) behaves the same.
#[test]
fn test_cold_replay_clears() {
    let frame = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
        "todos": [{"content": "done", "status": "completed"}]
    })));
    for _ in 0..3 {
        let mut state = TodoState::default();
        state.set_replaying_history(true);
        state.update(from_ref(&frame), 0, false);
        assert!(state.items.is_empty());
        assert!(state.completion_at.is_empty());
        assert!(!state.prune(Instant::now()), "nothing left to prune");
    }
}

/// A resume replays the transcript across several frames, not one. The
/// all-completed list that lands on a later replayed frame still reads as
/// cold: the partial list an earlier frame installed clears instead of
/// flashing the finished list with a fresh timestamp.
#[test]
fn test_replay_later_frame_cold() {
    let open = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
        "todos": [
            {"content": "one", "status": "in_progress"},
            {"content": "two", "status": "pending"}
        ]
    })));
    let partial = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
        "todos": [
            {"content": "one", "status": "completed"},
            {"content": "two", "status": "pending"}
        ]
    })));
    let finished = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
        "todos": [
            {"content": "one", "status": "completed"},
            {"content": "two", "status": "completed"}
        ]
    })));
    let mut frames: Vec<TranscriptFrame> = Vec::new();
    let mut state = TodoState::default();
    state.set_replaying_history(true);

    frames.push(open);
    state.update(&frames, 0, false);
    assert_eq!(state.items.len(), 2);

    frames.push(partial);
    state.update(&frames, 0, false);
    assert_eq!(state.items.len(), 2);

    frames.push(finished);
    state.update(&frames, 0, false);
    assert!(state.items.is_empty());
    assert!(state.completion_at.is_empty());
}

/// A rewind truncates the transcript below the cursor. The rollback
/// drops items and completion timestamps together, so the replayed
/// history cannot re-enter the completion visibility window.
#[test]
fn test_rewind_clears_completion() {
    let frame = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
        "todos": [{"content": "done", "status": "completed"}]
    })));
    let mut state = TodoState::default();
    state.update(from_ref(&frame), 0, true);
    assert!(state.completion_at.contains_key("done"));

    state.set_cursor(4);
    state.set_replaying_history(true);
    state.update(from_ref(&frame), 0, false);
    assert!(state.items.is_empty());
    assert!(state.completion_at.is_empty());
}

/// A durable-anchored cursor cannot be resolved against the generic frame
/// slice, so the accumulator re-derives from the front; the usize test
/// accessor reads a durable cursor back as zero.
#[test]
fn test_durable_cursor_re_derives() {
    let frame = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
        "todos": [{"content": "done", "status": "completed"}]
    })));
    let mut state = TodoState {
        cursor: Some(EventCursor::Durable("e7".into())),
        ..Default::default()
    };
    assert_eq!(
        state.cursor(),
        0,
        "the usize accessor reads a durable cursor as zero"
    );
    state.update(from_ref(&frame), 0, true);
    assert_eq!(
        state.items.len(),
        1,
        "a durable cursor re-derives from the front"
    );
}

/// A live run reaching the all-completed state records timestamps and
/// keeps the list visible for the grace window.
#[test]
fn test_live_done_timestamped() {
    let frame = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
        "todos": [{"content": "done", "status": "completed"}]
    })));
    let mut state = TodoState::default();
    state.update(from_ref(&frame), 0, true);

    assert!(state.completion_at.contains_key("done"));
    assert!(!state.prune(Instant::now()));
}

/// An idle replay with open work stays visible; only the completion
/// timestamp recording is suppressed.
#[test]
fn test_replay_open_kept() {
    let frame = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
        "todos": [
            {"content": "done", "status": "completed"},
            {"content": "open", "status": "pending"}
        ]
    })));
    let mut state = TodoState::default();
    state.set_replaying_history(true);
    state.update(from_ref(&frame), 0, false);

    assert_eq!(state.items.len(), 2);
    assert!(state.completion_at.is_empty());
    assert!(!state.prune(Instant::now()));
}

/// A live run completing the last open item after a restored mixed list
/// records timestamps for the whole list (one shared grace window);
/// the restore itself recorded none.
#[test]
fn test_live_completion_after_restore() {
    let mixed = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
        "todos": [
            {"content": "old", "status": "completed"},
            {"content": "current", "status": "in_progress"}
        ]
    })));
    let mut state = TodoState::default();
    state.set_replaying_history(true);
    state.update(from_ref(&mixed), 0, false);
    assert!(state.completion_at.is_empty());
    state.set_replaying_history(false);

    let finished = TranscriptFrame::Session(todo_write_frame(serde_json::json!({
        "todos": [
            {"content": "old", "status": "completed"},
            {"content": "current", "status": "completed"}
        ]
    })));
    let frames = vec![mixed, finished];
    state.update(&frames, 0, true);

    assert!(state.completion_at.contains_key("current"));
    assert!(state.completion_at.contains_key("old"));
}

mod verify;
