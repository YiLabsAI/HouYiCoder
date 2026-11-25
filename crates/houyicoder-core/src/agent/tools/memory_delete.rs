//! Structured memory-delete tool with explicit scope selection on the primary
//! agent and pinned auto-scope operation in background consolidation.

use std::sync::Arc;

use houyicoder_api::agent_event::MemoryOperation;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_async::PFut;
use houyicoder_context::{MemoryError, MemoryOrigin, MemoryScope};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};
use crate::agent::memory::MutationLog;

/// Structured memory-delete tool.
pub struct MemoryDeleteTool {
    provider: Arc<dyn MemoryProvider>,
    recorder: Option<Arc<MutationLog>>,
    pinned_scope: Option<MemoryScope>,
    origin: MemoryOrigin,
}

impl MemoryDeleteTool {
    /// Construct an unpinned delete tool that requires an explicit scope.
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self {
            provider,
            recorder: None,
            pinned_scope: None,
            origin: MemoryOrigin::PrimaryAgent,
        }
    }

    /// Construct a delete tool pinned to a specific storage scope.
    pub fn new_pinned(provider: Arc<dyn MemoryProvider>, scope: MemoryScope) -> Self {
        Self {
            provider,
            recorder: None,
            pinned_scope: Some(scope),
            origin: MemoryOrigin::PrimaryAgent,
        }
    }

    /// Attach a mutation recorder.
    pub(crate) fn with_recorder(mut self, recorder: Arc<MutationLog>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// Tag every delete with the given writer origin.
    pub fn with_origin(mut self, origin: MemoryOrigin) -> Self {
        self.origin = origin;
        self
    }
}

impl Tool for MemoryDeleteTool {
    fn name(&self) -> &str {
        "memory_delete"
    }
    fn description(&self) -> &str {
        "Permanently delete one stored memory by key. Use for contradicted or \
         wrong entries; prefer memory_archive for decayed entries that may \
         still be useful later."
    }
    fn input_schema(&self) -> Value {
        if self.pinned_scope.is_some() {
            json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "The kebab-case key (file stem) of the memory to delete."
                    }
                },
                "required": ["key"],
                "additionalProperties": false
            })
        } else {
            json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "The kebab-case key (file stem) of the memory to delete."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["user", "project", "auto"],
                        "description": "The storage scope holding the memory to delete."
                    }
                },
                "required": ["key", "scope"],
                "additionalProperties": false
            })
        }
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let provider = Arc::clone(&self.provider);
        let recorder = self.recorder.clone();
        let pinned_scope = self.pinned_scope;
        let origin = self.origin;
        Box::pin(async move {
            let key = input
                .get("key")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .ok_or_else(|| {
                    ToolError::Failed("memory_delete: 'key' must be a non-empty string".to_string())
                })?;
            let scope = match pinned_scope {
                Some(pinned) => pinned,
                None => {
                    let label = input.get("scope").and_then(|v| v.as_str()).ok_or_else(|| {
                        ToolError::Failed(
                            "memory_delete: 'scope' must be user, project, or auto".to_string(),
                        )
                    })?;
                    MemoryScope::from_label(label).ok_or_else(|| {
                        ToolError::Failed(format!(
                            "memory_delete: unknown scope '{label}'; use user, project, or auto"
                        ))
                    })?
                }
            };
            match provider.delete_memory_in_scope(key, scope, origin) {
                Ok(()) => {
                    if let Some(recorder) = &recorder {
                        recorder.record(key, MemoryOperation::Deleted, scope);
                    }
                    Ok(json!({"deleted": key, "scope": scope.as_label()}))
                }
                Err(MemoryError::NotFound) => Err(ToolError::Failed(format!(
                    "memory_delete: no memory with key '{key}' in scope '{}'",
                    scope.as_label()
                ))),
                Err(e) => Err(ToolError::Failed(format!("memory_delete: {e}"))),
            }
        })
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn requires_approval(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests;
