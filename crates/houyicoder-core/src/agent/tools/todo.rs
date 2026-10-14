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
mod tests;
