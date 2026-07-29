//! The EnterWorktree tool: a passive capability the model invokes only when
//! the user explicitly asks to work in a worktree. Creates (or resumes) a
//! linked worktree, narrows the sandbox fence + cwd into it. Delegates to the
//! WorktreeController. Declared non-concurrency-safe so the agent loop
//! serializes it (a cwd + fence switch is process-global state; a parallel
//! tool dispatch in the same turn would race it).

use std::sync::Arc;

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use serde_json::{Value, json};

use crate::agent::worktree_controller::WorktreeController;

pub struct EnterWorktreeTool {
    controller: Arc<WorktreeController>,
}

impl EnterWorktreeTool {
    pub fn new(controller: Arc<WorktreeController>) -> Self {
        Self { controller }
    }
}

const DESCRIPTION: &str = "\
Create an isolated git worktree and switch the session into it. \
Use this tool ONLY when the user explicitly asks to work in a worktree \
(start a worktree, work in a worktree, create a worktree, use a worktree). \
Do NOT call it for plain branch work or feature work — use normal git \
commands then. The worktree is created under the project state dir on a new \
branch based on HEAD. The session cwd + sandbox fence narrow to the worktree \
so the agent edits the isolated copy, not the main working tree. Use \
exit_worktree to leave (keep or remove).";

impl Tool for EnterWorktreeTool {
    fn name(&self) -> &str {
        "enter_worktree"
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Optional name for the worktree. Each slash-separated segment may contain only letters, digits, dots, underscores, and dashes; max 64 chars. A random name is generated when not provided."
                }
            }
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let controller = Arc::clone(&self.controller);
        Box::pin(async move {
            let name = input.get("name").and_then(|v| v.as_str()).map(String::from);
            let result = controller
                .enter(name)
                .await
                .map_err(|e| ToolError::Failed(e.to_string()))?;
            Ok(json!({
                "worktree_path": result.worktree_path,
                "worktree_branch": result.worktree_branch,
                "message": result.message,
            }))
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
    fn requires_approval(&self) -> bool {
        false
    }
}
