//! Checklist view model accumulated from the wire stream. The agent's
//! todo-write tool calls ride the transcript as ordinary tool calls whose
//! input carries the new task list; this module parses that wire payload
//! into a typed view model the render layer reads. The data is already on
//! the wire (the transcript renders tool calls from the same input), so
//! this is client-side view derivation, not engine-state coupling.
//!
//! Last-write-wins: the tool posts the full list each call, so the most
//! recent todo-write frame determines the current checklist. The accumulator
//! re-scans the full frame list each rebuild, so out-of-order or replayed
//! frames stay consistent without a cursor.

use houyicoder_protocol::frontend::session_update::{SessionUpdate, ToolCall};

/// The three lifecycle states a checklist entry cycles through. Follows the
/// wire vocabulary (pending, in_progress, completed) without importing the
/// engine task type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// Parse the snake-case status string the tool input carries. Unknown
    /// values degrade to Pending so a forward-incompatible payload never hides
    /// a task from the view.
    pub fn from_snake(s: &str) -> Self {
        match s {
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            _ => Self::Pending,
        }
    }
}

/// One checklist entry: the content line, its status, and the optional
/// active-form label shown for the in-progress task (a short verb phrase
/// describing the work underway, e.g. running tests for a run-tests task).
#[derive(Debug, Clone)]
pub struct TodoView {
    pub content: String,
    pub status: TodoStatus,
    pub active_form: Option<String>,
}

/// Parse a todo-write tool call's input into the view list. The input shape
/// matches what the tool itself parses on the engine side: a todos array
/// whose items carry content, status, and an optional activeForm. Returns
/// None when the frame is not a todo-write call; Some(vec) when it is (the
/// vec may be empty when the tool's all-done-clears behavior posted an empty
/// list). This distinction lets the accumulator apply last-write-wins only
/// to genuine checklist frames.
pub fn from_tool_call(update: &SessionUpdate) -> Option<Vec<TodoView>> {
    let SessionUpdate::ToolCall(ToolCall {
        title, raw_input, ..
    }) = update
    else {
        return None;
    };
    if title != "todo_write" {
        return None;
    }
    let arr = raw_input
        .as_ref()
        .and_then(|v| v.get("todos"))
        .and_then(|v| v.as_array());
    let Some(arr) = arr else {
        // A todo-write call with no recognized todos array: treat as an
        // empty checklist update so the view clears, matching the tool's
        // own degrade on malformed input.
        return Some(Vec::new());
    };
    let views = arr
        .iter()
        .filter_map(|item| {
            let content = item.get("content")?.as_str()?.to_string();
            if content.is_empty() {
                return None;
            }
            let status = item
                .get("status")
                .and_then(|v| v.as_str())
                .map(TodoStatus::from_snake)
                .unwrap_or(TodoStatus::Pending);
            let active_form = item
                .get("activeForm")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            Some(TodoView {
                content,
                status,
                active_form,
            })
        })
        .collect();
    Some(views)
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn test_tool_call_without_payload() {
        let mut tc = ToolCall::new(ToolCallId::new("c1"), "todo_write");
        tc.raw_input = None;
        let views = from_tool_call(&SessionUpdate::ToolCall(tc)).expect("todo-write frame");
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
}
