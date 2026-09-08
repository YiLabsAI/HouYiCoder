//! MultiEditTool: apply multiple edits atomically to one file. Each edit
//! is {old_string, new_string, replace_all?} applied in order to the
//! in-memory content; any failed edit aborts the whole batch with NO
//! write (all-or-nothing). Destructive, so approval is required. Split
//! from tools.rs so the module root stays under the file-size gate.

use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_async::PFut;
use serde_json::{Value, json};

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_protocol::extension::ToolError;

use super::file_edit::{apply_one, read_editable_text};
use crate::agent::conditional_activation::ConditionalSkillActivator;
use crate::agent::diff::unified_diff;

/// Apply multiple edits atomically to one file. Each edit is {old_string,
/// new_string, replace_all?} applied in order to the in-memory content;
/// any failed edit (0/multi/no-op) aborts the whole batch with NO write
/// (all-or-nothing). Single-file atomic batch now (multi-file
/// transactions are a TODO). Returns a unified diff of original to final.
pub struct MultiEditTool {
    session: Arc<dyn SandboxSession>,
    activator: Option<Arc<dyn ConditionalSkillActivator>>,
}

impl MultiEditTool {
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

impl Tool for MultiEditTool {
    fn name(&self) -> &str {
        "multiedit"
    }
    fn description(&self) -> &str {
        "Apply multiple edits atomically to one file. \
         Input: {path, edits: [{old_string, new_string, replace_all?}]}. \
         All-or-nothing: any failed edit rolls back (no write). \
         Returns a unified diff of original to final."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {"type": "string"},
                            "new_string": {"type": "string"},
                            "replace_all": {"type": "boolean"}
                        },
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let session = self.session.clone();
        let activator = self.activator.clone();
        Box::pin(async move {
            let path = input.get("path").and_then(|v| v.as_str()).ok_or_else(|| {
                ToolError::InvalidInput("multiedit: path (string) required".into())
            })?;
            // Activate paths-gated skills by intent, before the edit.
            if let Some(a) = &activator {
                a.activate_for_paths(&[path.to_string()]);
            }
            let edits = input
                .get("edits")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    ToolError::InvalidInput("multiedit: edits (array) required".into())
                })?;
            if edits.is_empty() {
                return Err(ToolError::InvalidInput(
                    "multiedit: edits must be non-empty".into(),
                ));
            }
            // Read once; apply all edits in memory; write once on full success.
            let original = read_editable_text(&session, path).await?;
            let mut content = original.clone();
            let mut applied = 0u32;
            for (i, e) in edits.iter().enumerate() {
                let old = e
                    .get("old_string")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidInput(format!(
                            "multiedit: edits[{i}].old_string required"
                        ))
                    })?;
                let new = e
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidInput(format!(
                            "multiedit: edits[{i}].new_string required"
                        ))
                    })?;
                let replace_all = e
                    .get("replace_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                content = apply_one(&content, old, new, replace_all)
                    .map_err(|err| ToolError::Failed(format!("multiedit: edits[{i}]: {err}")))?;
                applied += 1;
            }
            let diff = unified_diff(&original, &content, 3);
            session
                .write_file(path, content.into_bytes())
                .await
                .map_err(|e| ToolError::Failed(format!("multiedit: {e}")))?;
            Ok(json!({
                "path": path,
                "diff": diff,
                "edits_applied": applied,
                "bytes": original.len(),
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
