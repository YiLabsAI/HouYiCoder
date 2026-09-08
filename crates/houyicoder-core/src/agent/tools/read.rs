//! ReadTool: read a file from the sandbox workspace. Read-only, so no
//! approval is needed. Split from tools.rs so the module root stays under
//! the file-size gate.

use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_async::PFut;
use serde_json::{Value, json};

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_protocol::extension::ToolError;

use crate::agent::conditional_activation::ConditionalSkillActivator;

/// Read a file from the sandbox workspace. Read-only, no approval.
pub struct ReadTool {
    session: Arc<dyn SandboxSession>,
    activator: Option<Arc<dyn ConditionalSkillActivator>>,
}

impl ReadTool {
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

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read a file from the sandbox workspace. Input: {path: string, max_bytes?: number}. Returns content (utf-8, truncated at max_bytes)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "max_bytes": {"type": "number"}
            },
            "required": ["path"]
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let session = self.session.clone();
        let activator = self.activator.clone();
        Box::pin(async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("read: path (string) required".into()))?;
            // Activate paths-gated skills by intent, before the read.
            if let Some(a) = &activator {
                a.activate_for_paths(&[path.to_string()]);
            }
            let max = input
                .get("max_bytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(65_536) as usize;
            // A zero budget truncates to an empty read, which the transcript
            // would mislabel "Read 0 lines (empty file)" for a non-empty file.
            // Reject it so the cause surfaces as an error, not a silent body.
            validate_read_max_bytes(max)?;
            let bytes = session
                .read_file(path, max)
                .await
                .map_err(|e| ToolError::Failed(format!("read: {e}")))?;
            Ok(json!({
                "path": path,
                "content": String::from_utf8_lossy(&bytes),
                "truncated": bytes.len() == max,
            }))
        })
    }
    fn is_destructive(&self) -> bool {
        false
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn requires_approval(&self) -> bool {
        false
    }
}

/// Reject a zero read budget so the tool errors instead of returning an
/// empty body that the transcript would mislabel "Read 0 lines (empty
/// file)" for a non-empty file. Pure so the gate is unit-testable without
/// a sandbox session.
fn validate_read_max_bytes(max: usize) -> Result<(), ToolError> {
    if max == 0 {
        return Err(ToolError::InvalidInput(
            "read: max_bytes must be greater than 0".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
