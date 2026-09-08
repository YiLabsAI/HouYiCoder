//! EditTool: edit a file by replacing old_string with new_string (exact
//! match). Destructive, so approval is required. Split from tools.rs so
//! the module root stays under the file-size gate.

use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_async::PFut;
use serde_json::{Value, json};

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_protocol::extension::ToolError;

use super::file_edit::apply_edit;
use crate::agent::conditional_activation::ConditionalSkillActivator;

/// Edit a file by replacing old_string with new_string. Strict exact
/// matching (no fuzzy replacers — fuzzy matching is too error-prone).
/// Fail-closed: 0 matches, multiple matches without replace_all, empty
/// old_string, and no-op edits are all refused before any write. Text
/// files only (non-utf-8 -> error). Returns a unified diff of the change
/// so the model and the approval UI see exactly what changed. Destructive
/// -> approval-gated.
pub struct EditTool {
    session: Arc<dyn SandboxSession>,
    activator: Option<Arc<dyn ConditionalSkillActivator>>,
}

impl EditTool {
    pub fn new(session: Arc<dyn SandboxSession>) -> Self {
        Self {
            session,
            activator: None,
        }
    }
    pub fn with_activator(mut self, activator: Option<Arc<dyn ConditionalSkillActivator>>) -> Self {
        self.activator = activator;
        self
    }
}

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Edit a file by replacing old_string with new_string (exact match). \
         Input: {path, old_string, new_string, replace_all?}. \
         old_string must be unique unless replace_all. \
         Refused: 0 matches, multiple matches without replace_all, empty old_string, no-op. \
         Returns a unified diff. Text files only."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let session = self.session.clone();
        let activator = self.activator.clone();
        Box::pin(async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("edit: path (string) required".into()))?;
            // Activate paths-gated skills by intent, before the edit.
            if let Some(a) = &activator {
                a.activate_for_paths(&[path.to_string()]);
            }
            let old = input
                .get("old_string")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ToolError::InvalidInput("edit: old_string (string) required".into())
                })?;
            let new = input
                .get("new_string")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ToolError::InvalidInput("edit: new_string (string) required".into())
                })?;
            let replace_all = input
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let (diff, n, bytes) = apply_edit(&session, path, old, new, replace_all).await?;
            Ok(json!({
                "path": path,
                "diff": diff,
                "occurrences_replaced": n,
                "bytes": bytes,
            }))
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn requires_approval(&self) -> bool {
        true
    }
}
