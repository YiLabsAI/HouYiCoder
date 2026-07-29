//! Session-scoped task checklist tool — full-list-replace semantics.
//!
//! The model sends the entire desired todo list each call (not incremental
//! edits); the tool atomically swaps the old list for the new one under a
//! mutex and returns both for observability. Three statuses track progress:
//! pending, in_progress, completed. When every item is completed the list
//! clears to empty — the done signal — so the agent and the host know the
//! session's work is finished without scanning statuses.
//!
//! State is held inside the tool as a shared mutex-protected vector. The
//! registry holds the tool behind an arc, so the state lives as long as the
//! registry (one per runner, one per session). This is the seam: the trait
//! does not carry session state, so the tool instance IS the state holder.
//! A future host-side accessor can clone the shared handle to render the
//! checklist in the UI without calling execute.
//!
//! Not concurrency-safe: a full-list-replace races if two calls overlap (one
//! list silently clobbers the other). The loop dispatches tools sequentially
//! today; when parallel dispatch lands, this flag keeps todo writes on the
//! sequential path. Not destructive: the state is in-memory and reversible
//! (the old list is returned in every result). No approval needed: managing a
//! checklist is meta-planning, not a side effect on the workspace.

use std::sync::{Arc, Mutex};

use houyicoder_async::PFut;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};

/// A single task in the checklist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodoItem {
    /// Imperative form: what needs to be done (e.g. Run tests).
    pub content: String,
    /// Lifecycle state of the task.
    pub status: TodoStatus,
    /// Present-continuous form shown while the task is active (e.g. Running
    /// tests). Optional — callers may omit it, but the description encourages
    /// providing it so the host can render a live progress label.
    pub active_form: Option<String>,
}

/// The lifecycle state of a todo item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// The wire string used in the input and output JSON.
    fn as_str(&self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Completed => "completed",
        }
    }

    /// Parse a wire string into a status. Rejects unknown values with a clear
    /// error the model can see and correct.
    fn parse(s: &str) -> Result<Self, ToolError> {
        match s {
            "pending" => Ok(TodoStatus::Pending),
            "in_progress" => Ok(TodoStatus::InProgress),
            "completed" => Ok(TodoStatus::Completed),
            _ => Err(ToolError::Failed(format!(
                "todo: status must be pending, in_progress, or completed; got '{s}'"
            ))),
        }
    }
}

/// A session-scoped task checklist tool. Holds the full list under a mutex;
/// each call replaces the list atomically and returns the old and new state.
pub struct TodoWriteTool {
    state: Arc<Mutex<Vec<TodoItem>>>,
}

impl TodoWriteTool {
    /// Create a tool with an empty checklist.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Create a tool with a shared state handle. The host can hold a clone of
    /// the same arc to read the checklist for UI rendering without dispatching
    /// a tool call. Two tools built from the same handle share one list.
    pub fn with_shared_state(state: Arc<Mutex<Vec<TodoItem>>>) -> Self {
        Self { state }
    }

    /// A clone of the internal state handle, for hosts that want to observe
    /// the checklist directly (e.g. a status bar or a /todos command).
    pub fn state_handle(&self) -> Arc<Mutex<Vec<TodoItem>>> {
        Arc::clone(&self.state)
    }
}

impl Default for TodoWriteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todo_write"
    }
    fn description(&self) -> &str {
        "Create or update the session task checklist. Send the FULL desired \
         list each call (not incremental edits). Each item has content \
         (imperative), status (pending | in_progress | completed), and \
         optional active_form (present-continuous, shown while active). \
         Keep exactly one item in_progress at a time. When all items are \
         completed the list clears automatically. Use proactively for \
         multi-step tasks (3+ steps) to track progress."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                            "activeForm": {"type": "string"}
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["todos"],
            "additionalProperties": false
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let new_todos = parse_todos(&input)?;
            let (old_todos, stored_todos, cleared) = {
                let mut guard = state
                    .lock()
                    .map_err(|e| ToolError::Failed(format!("todo: state lock poisoned: {e}")))?;
                let old = guard.clone();
                // When every item is completed, clear the list to empty.
                // This is the done signal: the host can detect completion by
                // checking cleared rather than scanning statuses.
                let all_done = !new_todos.is_empty()
                    && new_todos.iter().all(|t| t.status == TodoStatus::Completed);
                if all_done {
                    guard.clear();
                    (old, Vec::new(), true)
                } else {
                    let stored = new_todos.clone();
                    *guard = stored.clone();
                    (old, stored, false)
                }
            };
            Ok(render_result(&old_todos, &stored_todos, cleared))
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn is_destructive(&self) -> bool {
        false
    }
}

/// Parse the full todo list from the tool input. Validates that todos is an
/// array and each item has a non-empty content and a valid status.
fn parse_todos(input: &Value) -> Result<Vec<TodoItem>, ToolError> {
    let arr = input
        .get("todos")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ToolError::InvalidInput("todo: todos (array) required".into()))?;
    let mut items = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let content = item
            .get("content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ToolError::Failed(format!(
                    "todo: item {i} content (non-empty string) required"
                ))
            })?;
        let status_str = item.get("status").and_then(|v| v.as_str()).ok_or_else(|| {
            ToolError::InvalidInput(format!("todo: item {i} status (string) required"))
        })?;
        let status = TodoStatus::parse(status_str)?;
        let active_form = item
            .get("activeForm")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        items.push(TodoItem {
            content: content.to_string(),
            status,
            active_form,
        });
    }
    Ok(items)
}

/// Render the tool result as structured JSON. Returns the old and new lists
/// plus a count summary, so the host can render a rich checklist view and
/// the agent can audit what changed. The cleared flag signals that the
/// all-done-clears mechanism fired.
fn render_result(old_todos: &[TodoItem], new_todos: &[TodoItem], cleared: bool) -> Value {
    let old = old_todos.iter().map(item_to_json).collect::<Vec<_>>();
    let new = new_todos.iter().map(item_to_json).collect::<Vec<_>>();
    let counts = count_statuses(new_todos);
    json!({
        "todos": new,
        "old_todos": old,
        "total": new_todos.len(),
        "pending": counts.pending,
        "in_progress": counts.in_progress,
        "completed": counts.completed,
        "cleared": cleared,
    })
}

/// Serialize a single todo item to a JSON object matching the wire schema.
fn item_to_json(item: &TodoItem) -> Value {
    let mut obj = json!({
        "content": item.content,
        "status": item.status.as_str(),
    });
    if let Some(ref active) = item.active_form {
        obj["activeForm"] = json!(active);
    }
    obj
}

/// Named status counts. A tuple would let a return-order swap compile
/// silently; the named struct makes field assignment unambiguous.
struct StatusCounts {
    pending: usize,
    in_progress: usize,
    completed: usize,
}

fn count_statuses(todos: &[TodoItem]) -> StatusCounts {
    let mut pending = 0;
    let mut in_progress = 0;
    let mut completed = 0;
    for t in todos {
        match t.status {
            TodoStatus::Pending => pending += 1,
            TodoStatus::InProgress => in_progress += 1,
            TodoStatus::Completed => completed += 1,
        }
    }
    StatusCounts {
        pending,
        in_progress,
        completed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pollster::block_on;

    /// Helper: build a todo input JSON with the given items.
    fn input_json(items: &[(&str, &str)]) -> Value {
        let todos: Vec<Value> = items
            .iter()
            .map(|(c, s)| json!({"content": c, "status": s, "activeForm": format!("{} {c}", s)}))
            .collect();
        json!({"todos": todos})
    }

    /// Helper: build a todo input without activeForm.
    fn input_json_bare(items: &[(&str, &str)]) -> Value {
        let todos: Vec<Value> = items
            .iter()
            .map(|(c, s)| json!({"content": c, "status": s}))
            .collect();
        json!({"todos": todos})
    }

    #[test]
    fn test_empty_list_replaces_state() {
        let tool = TodoWriteTool::new();
        let out = block_on(tool.execute(
            ToolCtx::new("test"),
            input_json(&[("write code", "pending")]),
        ))
        .unwrap();
        assert_eq!(out["total"], 1);
        assert_eq!(out["pending"], 1);
        // Second call with empty list clears.
        let out = block_on(tool.execute(ToolCtx::new("test"), json!({"todos": []}))).unwrap();
        assert_eq!(out["total"], 0);
        assert_eq!(out["old_todos"].as_array().unwrap().len(), 1);
        assert!(!out["cleared"].as_bool().unwrap());
    }

    #[test]
    fn test_all_completed_clears_list() {
        let tool = TodoWriteTool::new();
        // Seed with three pending items.
        block_on(tool.execute(
            ToolCtx::new("test"),
            input_json(&[
                ("task a", "pending"),
                ("task b", "pending"),
                ("task c", "pending"),
            ]),
        ))
        .unwrap();
        // Mark all completed.
        let out = block_on(tool.execute(
            ToolCtx::new("test"),
            input_json(&[
                ("task a", "completed"),
                ("task b", "completed"),
                ("task c", "completed"),
            ]),
        ))
        .unwrap();
        assert!(out["cleared"].as_bool().unwrap());
        assert_eq!(out["total"], 0);
        // The stored state is now empty.
        let handle = tool.state_handle();
        let guard = handle.lock().unwrap();
        assert!(guard.is_empty());
    }

    #[test]
    fn test_partial_completion_keeps_list() {
        let tool = TodoWriteTool::new();
        let out = block_on(tool.execute(
            ToolCtx::new("test"),
            input_json(&[
                ("done item", "completed"),
                ("active item", "in_progress"),
                ("later item", "pending"),
            ]),
        ))
        .unwrap();
        assert!(!out["cleared"].as_bool().unwrap());
        assert_eq!(out["completed"], 1);
        assert_eq!(out["in_progress"], 1);
        assert_eq!(out["pending"], 1);
        assert_eq!(out["total"], 3);
    }

    #[test]
    fn test_old_todos_returned() {
        let tool = TodoWriteTool::new();
        block_on(tool.execute(ToolCtx::new("test"), input_json(&[("first", "pending")]))).unwrap();
        let out = block_on(tool.execute(
            ToolCtx::new("test"),
            input_json(&[("second", "in_progress")]),
        ))
        .unwrap();
        let old = out["old_todos"].as_array().unwrap();
        assert_eq!(old.len(), 1);
        assert_eq!(old[0]["content"], "first");
        assert_eq!(old[0]["status"], "pending");
        let new = out["todos"].as_array().unwrap();
        assert_eq!(new.len(), 1);
        assert_eq!(new[0]["content"], "second");
        assert_eq!(new[0]["status"], "in_progress");
    }

    #[test]
    fn test_active_form_optional() {
        let tool = TodoWriteTool::new();
        let out = block_on(tool.execute(
            ToolCtx::new("test"),
            input_json_bare(&[("task", "pending")]),
        ))
        .unwrap();
        let item = &out["todos"][0];
        assert_eq!(item["content"], "task");
        assert_eq!(item["status"], "pending");
        // activeForm absent from output when absent from input.
        assert!(item.get("activeForm").is_none() || item["activeForm"].is_null());
    }

    #[test]
    fn test_output_includes_active_form() {
        let tool = TodoWriteTool::new();
        let out = block_on(tool.execute(
            ToolCtx::new("test"),
            input_json(&[("run tests", "in_progress")]),
        ))
        .unwrap();
        assert_eq!(out["todos"][0]["activeForm"], "in_progress run tests");
    }

    #[test]
    fn test_rejects_empty_content() {
        let tool = TodoWriteTool::new();
        let result = block_on(tool.execute(
            ToolCtx::new("test"),
            json!({
                "todos": [{"content": "", "status": "pending"}]
            }),
        ));
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = err.message();
        assert!(msg.contains("content"));
    }

    #[test]
    fn test_rejects_invalid_status() {
        let tool = TodoWriteTool::new();
        let result = block_on(tool.execute(
            ToolCtx::new("test"),
            json!({
                "todos": [{"content": "task", "status": "blocked"}]
            }),
        ));
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = err.message();
        assert!(msg.contains("status"));
    }

    #[test]
    fn test_rejects_missing_todos_array() {
        let tool = TodoWriteTool::new();
        let result = block_on(tool.execute(ToolCtx::new("test"), json!({"items": []})));
        assert!(result.is_err());
    }

    #[test]
    fn test_rejects_non_array_todos() {
        let tool = TodoWriteTool::new();
        let result = block_on(tool.execute(ToolCtx::new("test"), json!({"todos": "not an array"})));
        assert!(result.is_err());
    }

    #[test]
    fn test_shared_state_across_clone() {
        let tool = TodoWriteTool::new();
        let handle = tool.state_handle();
        let tool2 = TodoWriteTool::with_shared_state(handle);
        block_on(tool.execute(
            ToolCtx::new("test"),
            input_json(&[("shared task", "pending")]),
        ))
        .unwrap();
        // tool2 sees the same state because it shares the arc.
        let handle2 = tool2.state_handle();
        let guard = handle2.lock().unwrap();
        assert_eq!(guard.len(), 1);
        assert_eq!(guard[0].content, "shared task");
    }

    #[test]
    fn test_state_accessible_via_handle() {
        let tool = TodoWriteTool::new();
        block_on(tool.execute(
            ToolCtx::new("test"),
            input_json(&[("a", "pending"), ("b", "in_progress")]),
        ))
        .unwrap();
        let handle = tool.state_handle();
        let guard = handle.lock().unwrap();
        assert_eq!(guard.len(), 2);
        assert_eq!(guard[0].status, TodoStatus::Pending);
        assert_eq!(guard[1].status, TodoStatus::InProgress);
    }

    #[test]
    fn test_capability_flags() {
        let tool = TodoWriteTool::new();
        // Full-list-replace races under concurrent calls, so not safe.
        assert!(!tool.is_concurrency_safe());
        // Mutates session state.
        assert!(!tool.is_read_only());
        // In-memory and reversible (old list returned every call).
        assert!(!tool.is_destructive());
        // Meta-planning, no approval needed.
        assert!(!tool.requires_approval());
    }

    #[test]
    fn test_full_replace_not_incremental() {
        let tool = TodoWriteTool::new();
        block_on(tool.execute(
            ToolCtx::new("test"),
            input_json(&[("a", "pending"), ("b", "pending"), ("c", "pending")]),
        ))
        .unwrap();
        // A replace with only one item drops the other two entirely.
        let out = block_on(tool.execute(
            ToolCtx::new("test"),
            input_json(&[("only one", "in_progress")]),
        ))
        .unwrap();
        assert_eq!(out["total"], 1);
        assert_eq!(out["todos"][0]["content"], "only one");
        let handle = tool.state_handle();
        let guard = handle.lock().unwrap();
        assert_eq!(guard.len(), 1);
    }

    #[test]
    fn test_empty_list_not_done() {
        let tool = TodoWriteTool::new();
        // An empty list is not an all-done clear (nothing was completed).
        let out = block_on(tool.execute(ToolCtx::new("test"), json!({"todos": []}))).unwrap();
        assert!(!out["cleared"].as_bool().unwrap());
        assert_eq!(out["total"], 0);
    }

    #[test]
    fn test_schema_strict_object() {
        let tool = TodoWriteTool::new();
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["todos"]["type"], "array");
        let item_schema = &schema["properties"]["todos"]["items"];
        assert_eq!(item_schema["additionalProperties"], false);
        let required = item_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|r| r == "content"));
        assert!(required.iter().any(|r| r == "status"));
    }
}
