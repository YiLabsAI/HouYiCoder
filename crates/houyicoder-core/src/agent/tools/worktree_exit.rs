//! The ExitWorktree tool: leaves a worktree session created by enter_worktree.
//! keep preserves the worktree + branch on disk; remove deletes both. A
//! remove with uncommitted changes refuses unless discard_changes=true
//! (fail-closed — the controller lists the work so the user confirms). Like
//! enter, declared non-concurrency-safe so the loop serializes the fence +
//! cwd switch.

use std::sync::Arc;

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use serde_json::{Value, json};

use crate::agent::worktree_controller::{ExitAction, WorktreeController};

pub struct ExitWorktreeTool {
    controller: Arc<WorktreeController>,
}

impl ExitWorktreeTool {
    pub fn new(controller: Arc<WorktreeController>) -> Self {
        Self { controller }
    }
}

const DESCRIPTION: &str = "\
Exit a worktree session created by enter_worktree and return the session to \
the original working directory. Only operates on worktrees this session \
created with enter_worktree — a no-op when no worktree session is active. \
action=keep leaves the worktree and branch on disk (come back with cd); \
action=remove deletes both. remove with uncommitted files or commits not on \
the original branch refuses unless discard_changes=true — confirm with the \
user before re-invoking with true.";

impl Tool for ExitWorktreeTool {
    fn name(&self) -> &str {
        "exit_worktree"
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["keep", "remove"],
                    "description": "keep leaves the worktree and branch intact on disk; remove deletes both."
                },
                "discard_changes": {
                    "type": "boolean",
                    "description": "Only meaningful with action=remove. When true, discard uncommitted files and commits not on the original branch. When false (default), a remove with such work refuses and lists it."
                }
            },
            "required": ["action"]
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let controller = Arc::clone(&self.controller);
        Box::pin(async move {
            let action = input
                .get("action")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ToolError::InvalidInput("exit_worktree: action (keep|remove) required".into())
                })?;
            let action = match action {
                "keep" => ExitAction::Keep,
                "remove" => ExitAction::Remove,
                other => {
                    return Err(ToolError::InvalidInput(format!(
                        "exit_worktree: action must be keep or remove, got {other}"
                    )));
                }
            };
            let discard = input
                .get("discard_changes")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let outcome = controller
                .exit(action, discard)
                .await
                .map_err(|e| ToolError::Failed(e.to_string()))?;
            Ok(json!({
                "action": match outcome.action { ExitAction::Keep => "keep", ExitAction::Remove => "remove" },
                "original_cwd": outcome.original_cwd,
                "worktree_path": outcome.worktree_path,
                "worktree_branch": outcome.worktree_branch,
                "message": outcome.message,
            }))
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn is_read_only(&self) -> bool {
        false
    }
    /// Destructive only on remove — the gate asks the user for a remove.
    fn is_destructive(&self) -> bool {
        false
    }
    fn requires_approval(&self) -> bool {
        false
    }
    fn requires_approval_for(&self, input: &Value) -> bool {
        input
            .get("action")
            .and_then(|v| v.as_str())
            .map(|a| a == "remove")
            .unwrap_or(true)
    }
}
