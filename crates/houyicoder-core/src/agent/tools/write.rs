//! WriteTool: write a file in the sandbox workspace. Destructive, so
//! approval is required. Split from tools.rs so the module root stays
//! under the file-size gate.

use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_async::PFut;
use serde_json::{Value, json};

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_protocol::extension::ToolError;

use super::file_edit::EDIT_MAX_BYTES;
use crate::agent::conditional_activation::ConditionalSkillActivator;

/// Write a file in the sandbox workspace. Destructive, requires approval.
pub struct WriteTool {
    session: Arc<dyn SandboxSession>,
    activator: Option<Arc<dyn ConditionalSkillActivator>>,
}

impl WriteTool {
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

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Write a file in the sandbox workspace. Input: {path: string, content: string, write_if_unchanged?: bool}. Creates parent dirs. When write_if_unchanged is true and the existing bytes already equal content, the write is skipped (no mtime bump)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"},
                "write_if_unchanged": {"type": "boolean", "default": false}
            },
            "required": ["path", "content"]
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let session = self.session.clone();
        let activator = self.activator.clone();
        Box::pin(async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("write: path (string) required".into()))?;
            // Activate paths-gated skills by intent, before the write.
            if let Some(a) = &activator {
                a.activate_for_paths(&[path.to_string()]);
            }
            let content = input
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ToolError::InvalidInput("write: content (string) required".into())
                })?;
            let skip_if_unchanged = input
                .get("write_if_unchanged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // Read the existing bytes (if any) so should_skip_unchanged_write can decide.
            // A missing or unreadable file yields None — never a skip, so a
            // brand-new file is always created. The cap is content+1 (min the
            // standard edit cap): a larger existing file reads truncated and
            // cannot falsely match, so it falls through to a real write.
            let cap = content.len().saturating_add(1).max(EDIT_MAX_BYTES + 1);
            let existing = session.read_file(path, cap).await.ok();
            let lines = content.lines().count();
            if should_skip_unchanged_write(
                skip_if_unchanged,
                existing.as_deref(),
                content.as_bytes(),
            ) {
                return Ok(json!({
                    "path": path,
                    "bytes": content.len(),
                    "lines": lines,
                    "unchanged": true
                }));
            }
            session
                .write_file(path, content.as_bytes().to_vec())
                .await
                .map_err(|e| ToolError::Failed(format!("write: {e}")))?;
            Ok(json!({"path": path, "bytes": content.len(), "lines": lines}))
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

/// Decide whether a Write can be skipped: only when the caller asked for
/// write-if-unchanged AND the existing bytes exactly equal the new
/// content. A missing file (None) is never a skip — the write creates it.
/// Pure so the decision is unit-testable without a sandbox.
fn should_skip_unchanged_write(
    write_if_unchanged: bool,
    existing: Option<&[u8]>,
    content: &[u8],
) -> bool {
    write_if_unchanged && matches!(existing, Some(b) if b == content)
}

#[cfg(test)]
#[path = "write_tests.rs"]
mod tests;
