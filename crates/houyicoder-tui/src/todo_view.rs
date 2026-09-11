//! Checklist view model accumulated from the wire stream. The agent's
//! todo-write tool calls ride the transcript as ordinary tool calls whose
//! input carries the new task list; this module parses that wire payload
//! into a typed view model the render layer reads. The data is already on
//! the wire (the transcript renders tool calls from the same input), so
//! this is client-side view derivation, not engine-state coupling.
//!
//! Last-write-wins: the tool posts the full list each call, so the most
//! recent todo-write frame determines the current checklist. The accumulator
//! advances an append-only frame cursor; unrelated user boundaries retain the
//! last checklist instead of clearing it.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use houyicoder_protocol::frontend::session_update::{SessionUpdate, ToolCall};

use crate::transcript::TranscriptFrame;

/// Time a completed task remains visible before retiring from the transcript.
pub(crate) const RECENT_COMPLETION_TTL: Duration = Duration::from_secs(30);
const COMPLETED_LIST_TTL: Duration = Duration::from_secs(5);

/// Checklist status, including the paused state used for resumed idle work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Paused,
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

/// Projected checklist state and its completion visibility lifecycle.
#[derive(Default)]
pub struct TodoState {
    pub(crate) items: Vec<TodoView>,
    pub(crate) expanded: bool,
    pub(crate) completion_at: HashMap<String, Instant>,
    cursor: usize,
}

impl TodoState {
    /// Apply newly appended todo-write frames using last-write-wins semantics.
    pub(crate) fn update(&mut self, frames: &[TranscriptFrame], run_active: bool) {
        if self.cursor > frames.len() {
            self.cursor = 0;
            self.items.clear();
        }
        let mut latest = None;
        for frame in frames.iter().skip(self.cursor) {
            if let TranscriptFrame::Session(update) = frame
                && let Some(parsed) = from_tool_call(update)
            {
                latest = Some(parsed);
            }
        }
        self.cursor = frames.len();
        if !run_active {
            pause_inactive(&mut self.items);
        }
        let Some(mut items) = latest else {
            return;
        };
        let initial_projection = self.items.is_empty();
        if !run_active {
            pause_inactive(&mut items);
        }
        if !initial_projection {
            let old_completed: HashSet<String> = self
                .items
                .iter()
                .filter(|item| item.status == TodoStatus::Completed)
                .map(|item| item.content.clone())
                .collect();
            let now = Instant::now();
            for item in &items {
                if item.status == TodoStatus::Completed && !old_completed.contains(&item.content) {
                    self.completion_at.insert(item.content.clone(), now);
                }
            }
        }
        let completed: HashSet<String> = items
            .iter()
            .filter(|item| item.status == TodoStatus::Completed)
            .map(|item| item.content.clone())
            .collect();
        self.completion_at
            .retain(|content, _| completed.contains(content));
        if !items.is_empty()
            && items
                .iter()
                .all(|item| item.status == TodoStatus::Completed)
        {
            let now = Instant::now();
            for item in &items {
                self.completion_at
                    .entry(item.content.clone())
                    .or_insert(now);
            }
        }
        self.items = items;
    }

    /// Hide the whole checklist after every item has remained completed for the
    /// cohort visibility window.
    pub(crate) fn prune(&mut self, now: Instant) -> bool {
        if self.items.is_empty()
            || self
                .items
                .iter()
                .any(|item| item.status != TodoStatus::Completed)
            || self
                .completion_at
                .values()
                .max()
                .is_none_or(|completed| now.duration_since(*completed) < COMPLETED_LIST_TTL)
        {
            return false;
        }
        self.items.clear();
        self.completion_at.clear();
        self.expanded = false;
        true
    }

    /// Reset checklist content, expansion, timestamps, and frame cursor.
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    #[cfg(test)]
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    #[cfg(test)]
    pub(crate) fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor;
    }
}

fn pause_inactive(items: &mut [TodoView]) {
    for item in items {
        if item.status == TodoStatus::InProgress {
            item.status = TodoStatus::Paused;
        }
    }
}

/// Parse a todo-write tool call's input into the view list. The input shape
/// matches what the tool itself parses on the engine side: a todos array
/// whose items carry content, status, and an optional activeForm. Returns
/// None when the frame is not a valid todo-write update; Some(vec) when it is.
/// An explicit empty todos array clears the checklist, while malformed input
/// leaves the last valid state intact.
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
        tracing::warn!("ignored todo-write frame without a todos array");
        return None;
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
        state.update(std::slice::from_ref(&frame), true);
        assert_eq!(state.items[0].status, TodoStatus::InProgress);

        state.update(std::slice::from_ref(&frame), false);
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
}
