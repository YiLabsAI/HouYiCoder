//! Real-binary PTY tests for todo_write: the tool must render only through
//! the checklist widget, never as a raw todos-object transcript row. The
//! stub returns plain text by default, so these script a todo_write ToolCall
//! to drive the real tool through the binary.

#![allow(clippy::unwrap_in_result)]

use crate::common::{Key, RENDER_TIMEOUT, pty_session_scripted, pty_session_slow_scripted};

/// One todo_write call, then plain text so the run ends cleanly. The todo
/// carries the content the user saw leak as a raw JSON row.
const TODO_THEN_DONE: &str = r#"[
  [{"type":"ToolCall","id":"t1","name":"todo_write","input":{"todos":[{"content":"analyze the reported interaction","status":"in_progress"}]}}],
  [{"type":"Text","text":"done"}]
]"#;

/// A scripted todo_write renders in the checklist and never leaks its result
/// JSON into the transcript. The happy path (call frame present) is the
/// regression floor: any path that renders the todos object as a line is a bug.
#[test]
#[ignore]
fn test_todo_write_no_leak() {
    let mut s = pty_session_scripted(TODO_THEN_DONE);
    s.send_str("track this task");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("done", RENDER_TIMEOUT),
        "the run should finish: {}",
        s.output()
    );
    assert!(
        s.output().contains("analyze the reported interaction"),
        "the checklist should carry the todo: {}",
        s.output()
    );
    assert!(
        !s.output().contains("\"todos\":"),
        "the todo result must not leak as a raw row: {}",
        s.output()
    );
}

/// A todo_write followed by a slow streamed reply, interrupted mid-run, must
/// not leave the todo result rendering as a raw row through the rebuild.
#[test]
#[ignore]
fn test_todo_interrupt_no_leak() {
    let mut s = pty_session_slow_scripted(3000, TODO_THEN_DONE);
    s.send_str("track this task");
    s.send_key(&Key::Enter);
    // Wait until the todo_write executed and its checklist rendered, then
    // interrupt while the second (slow) response is still streaming.
    assert!(
        s.wait_for("analyze the reported interaction", RENDER_TIMEOUT),
        "the checklist should render before interrupt: {}",
        s.output()
    );
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for("Interrupted", RENDER_TIMEOUT),
        "Esc mid-run should interrupt: {}",
        s.output()
    );
    assert!(
        !s.output().contains("\"todos\":"),
        "the todo result must not leak as a raw row after interrupt: {}",
        s.output()
    );
}
