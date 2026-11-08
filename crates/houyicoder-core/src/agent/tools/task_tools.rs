//! The model-facing task tools over the parent's fleet of children. Four
//! verbs, one port: list the children, page through one child's output,
//! steer a running child or resume a finished one, and stop a running
//! child. They read the spawn handle the runtime attaches to every ToolCtx,
//! so a nested agent orchestrates its own children the same way the root
//! does.

use houyicoder_api::spawn::SpawnArgs;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use serde_json::{Value, json};

/// List every child the session delegates to, running and finished.
pub struct TaskListTool;

const LIST_DESCRIPTION: &str = "List the subagent tasks this session has delegated: running \
children and finished ones, with each child's id, type, description, status, and turns. Use \
this to see what is still in flight and what each id refers to before reading output or \
steering a task.

Input: {}.";

impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "task_list"
    }
    fn description(&self) -> &str {
        LIST_DESCRIPTION
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
    fn execute(&self, ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move {
            let Some(handle) = ctx.spawn_handle.as_ref() else {
                return Err(ToolError::Failed(
                    "task_list: no agent runtime is wired in this session".into(),
                ));
            };
            let Some(parent) = ctx.session_id else {
                return Err(ToolError::Failed(
                    "task_list: this dispatch is not bound to a session".into(),
                ));
            };
            let rows = handle.list_children(parent).await;
            if rows.is_empty() {
                return Ok(json!({"tasks": [], "note": "no delegated tasks"}));
            }
            let tasks: Vec<Value> = rows
                .iter()
                .map(|r| {
                    json!({
                        "task_id": r.child_session_id,
                        "subagent_type": r.subagent_type,
                        "description": r.description,
                        "status": r.status,
                        "turns": r.turn_count,
                        "elapsed_ms": r.elapsed_ms,
                    })
                })
                .collect();
            Ok(json!({"tasks": tasks}))
        })
    }
}

/// Page through one child's output: its status, summary, and a character
/// window of what it printed.
pub struct TaskOutputTool;

const OUTPUT_DESCRIPTION: &str = "Read one subagent task's output. Returns the task's status, \
its result summary when it has one, and a window of its output text — pass offset and limit \
to page through a long output. A finished task reads exactly what it answered; a running task \
reads what it has printed so far.

Input: {task_id: the child session id, offset?: character position to start at (default 0), \
limit?: characters to read (default 8000)}.";

impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "task_output"
    }
    fn description(&self) -> &str {
        OUTPUT_DESCRIPTION
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The child session id to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "Character position to start reading at. Defaults to 0."
                },
                "limit": {
                    "type": "integer",
                    "description": "Characters to read in this window. Defaults to 8000."
                }
            },
            "required": ["task_id"]
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move {
            let Some(handle) = ctx.spawn_handle.as_ref() else {
                return Err(ToolError::Failed(
                    "task_output: no agent runtime is wired in this session".into(),
                ));
            };
            let Some(parent) = ctx.session_id else {
                return Err(ToolError::Failed(
                    "task_output: this dispatch is not bound to a session".into(),
                ));
            };
            let task_id = string_field(&input, "task_id", "task_output")?;
            let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
            let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(8_000) as usize;
            let view = handle
                .child_output(parent, &task_id, offset, limit)
                .await
                .map_err(ToolError::Failed)?;
            let more = view.offset + view.text.chars().count() < view.total_chars;
            Ok(json!({
                "task_id": view.child_session_id,
                "status": view.status,
                "summary": view.summary,
                "output": view.text,
                "total_chars": view.total_chars,
                "offset": view.offset,
                "more": more,
            }))
        })
    }
}

/// Steer a running child with a mid-flight message, or resume a finished
/// child to continue on one.
pub struct SendMessageTool;

const SEND_DESCRIPTION: &str = "Send a message to a subagent task. A running child receives \
the message at its next turn boundary as additional instruction — use this to correct course \
or add context mid-flight. A finished child is resumed with the message as its next prompt, \
running in the background, so it continues its earlier conversation instead of starting \
over.

Input: {to: the child session id, message: the text to deliver}.";

impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }
    fn description(&self) -> &str {
        SEND_DESCRIPTION
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "The child session id to message"
                },
                "message": {
                    "type": "string",
                    "description": "The text to deliver to the child"
                }
            },
            "required": ["to", "message"]
        })
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn is_destructive(&self) -> bool {
        false
    }
    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move {
            let Some(handle) = ctx.spawn_handle.as_ref() else {
                return Err(ToolError::Failed(
                    "send_message: no agent runtime is wired in this session".into(),
                ));
            };
            let Some(parent) = ctx.session_id else {
                return Err(ToolError::Failed(
                    "send_message: this dispatch is not bound to a session".into(),
                ));
            };
            let to = string_field(&input, "to", "send_message")?;
            let message = string_field(&input, "message", "send_message")?;
            // The fleet list is the ownership check: an id this parent never
            // delegated is refused before any inbox or spawn is touched, so
            // one session cannot steer or resume another session's child.
            let rows = handle.list_children(parent).await;
            let child = rows
                .iter()
                .find(|r| r.child_session_id == to)
                .ok_or_else(|| {
                    ToolError::Failed(format!(
                        "send_message: task_id {to} is not a child of this session"
                    ))
                })?;
            let running = child.status == "running";
            // A running child takes the message at its next turn boundary.
            if running && handle.send_to_child_inbox(&to, message.clone()).is_ok() {
                return Ok(json!({
                    "task_id": to,
                    "delivered": "inbox",
                    "note": "the child receives the message at its next turn boundary",
                }));
            }
            // Not running (or the inbox refused): resume the child with the
            // message. The row carries the child's recorded type, so the
            // resumed boundary and runner identity name the same agent the
            // original spawn did rather than a placeholder.
            let mut args = SpawnArgs::new(
                child.subagent_type.clone(),
                message,
                "send_message".to_string(),
            );
            args.resume = Some(to.clone());
            args.run_in_background = true;
            let outcome = handle
                .spawn(&ctx, args)
                .await
                .map_err(|e| ToolError::Failed(format!("send_message: {e:?}")))?;
            Ok(json!({
                "task_id": to,
                "delivered": "resumed",
                "note": "the child was finished; it now continues in the background with the message",
                "child_session_id": outcome.child_session_id,
            }))
        })
    }
}

/// Stop a running child: its work ends and its concurrency slot frees.
pub struct TaskStopTool;

const STOP_DESCRIPTION: &str = "Stop a running subagent task. The child's current work ends, \
its concurrency slot frees, and its ending is recorded. A stopped child can be resumed later \
with send_message if its earlier turns are worth continuing. Does nothing to an already \
finished task.

Input: {task_id: the child session id to stop}.";

impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        "task_stop"
    }
    fn description(&self) -> &str {
        STOP_DESCRIPTION
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The child session id to stop"
                }
            },
            "required": ["task_id"]
        })
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn is_destructive(&self) -> bool {
        false
    }
    /// Stopping a child ends work the session already paid for, so an ask
    /// mode holds the stop for the user while an auto mode lets the model
    /// manage its own fleet.
    fn requires_approval(&self) -> bool {
        true
    }
    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move {
            let Some(handle) = ctx.spawn_handle.as_ref() else {
                return Err(ToolError::Failed(
                    "task_stop: no agent runtime is wired in this session".into(),
                ));
            };
            let Some(parent) = ctx.session_id else {
                return Err(ToolError::Failed(
                    "task_stop: this dispatch is not bound to a session".into(),
                ));
            };
            let task_id = string_field(&input, "task_id", "task_stop")?;
            // The fleet list is the ownership check: one session can stop
            // only the children it delegated, never another session's.
            let rows = handle.list_children(parent).await;
            if !rows.iter().any(|r| r.child_session_id == task_id) {
                return Err(ToolError::Failed(format!(
                    "task_stop: task_id {task_id} is not a child of this session"
                )));
            }
            if handle.kill_child(&task_id) {
                Ok(json!({
                    "task_id": task_id,
                    "stopped": true,
                }))
            } else {
                Ok(json!({
                    "task_id": task_id,
                    "stopped": false,
                    "note": "the task is not running",
                }))
            }
        })
    }
}

/// Read one required string field, with the tool name for the error.
fn string_field(input: &Value, field: &str, tool: &str) -> Result<String, ToolError> {
    input
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError::InvalidInput(format!("{tool}: {field} (string) required")))
}

#[cfg(test)]
mod tests;
