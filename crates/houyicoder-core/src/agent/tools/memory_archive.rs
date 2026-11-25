//! Structured memory-archive tool that moves an active memory into the
//! restorable archive directory without discarding its statistics.

use std::sync::Arc;

use houyicoder_api::agent_event::MemoryOperation;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_async::PFut;
use houyicoder_context::{MemoryError, MemoryOrigin, MemoryScope};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};
use crate::agent::memory::MutationLog;

/// Structured memory-archive tool.
pub struct MemoryArchiveTool {
    provider: Arc<dyn MemoryProvider>,
    recorder: Option<Arc<MutationLog>>,
    pinned_scope: Option<MemoryScope>,
    origin: MemoryOrigin,
}

impl MemoryArchiveTool {
    /// Construct an unpinned archive tool that accepts an optional scope.
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self {
            provider,
            recorder: None,
            pinned_scope: None,
            origin: MemoryOrigin::PrimaryAgent,
        }
    }

    /// Construct an archive tool pinned to a specific storage scope.
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

    /// Tag every archive call with the given writer origin.
    pub fn with_origin(mut self, origin: MemoryOrigin) -> Self {
        self.origin = origin;
        self
    }
}

impl Tool for MemoryArchiveTool {
    fn name(&self) -> &str {
        "memory_archive"
    }
    fn description(&self) -> &str {
        "Move one stored memory into the archive directory so it leaves the \
         active index and recall scan while remaining restorable via /memory."
    }
    fn input_schema(&self) -> Value {
        if self.pinned_scope.is_some() {
            json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "The kebab-case key (file stem) of the memory to archive."
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
                        "description": "The kebab-case key (file stem) of the memory to archive."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["user", "project", "auto"],
                        "description": "Optional scope holding the memory (defaults to auto)."
                    }
                },
                "required": ["key"],
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
                    ToolError::Failed(
                        "memory_archive: 'key' must be a non-empty string".to_string(),
                    )
                })?;
            let scope = match pinned_scope {
                Some(pinned) => pinned,
                None => match input.get("scope") {
                    Some(v) => {
                        let label = v.as_str().ok_or_else(|| {
                            ToolError::Failed(
                                "memory_archive: 'scope' must be user, project, or auto"
                                    .to_string(),
                            )
                        })?;
                        MemoryScope::from_label(label).ok_or_else(|| {
                            ToolError::Failed(format!(
                                "memory_archive: unknown scope '{label}'; use user, project, or auto"
                            ))
                        })?
                    }
                    None => MemoryScope::Auto,
                },
            };
            match provider.archive_memory(key, scope, origin) {
                Ok(()) => {
                    if let Some(recorder) = &recorder {
                        recorder.record(key, MemoryOperation::Archived, scope);
                    }
                    Ok(json!({"archived": key, "scope": scope.as_label()}))
                }
                Err(MemoryError::NotFound) => Err(ToolError::Failed(format!(
                    "memory_archive: no memory with key '{key}' in scope '{}'",
                    scope.as_label()
                ))),
                Err(e) => Err(ToolError::Failed(format!("memory_archive: {e}"))),
            }
        })
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

#[cfg(test)]
mod tests;
