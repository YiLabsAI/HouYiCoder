//! Scope-flow tools the forked consolidation dream uses to move a topic
//! between the auto and project scopes. The tool routes a structured call to
//! the provider, which owns the file move and index rebuild, so the tool
//! holds no path logic and offers no path-escape surface. The project scope
//! outranks auto on a key collision, so a promoted topic survives an
//! automatic rewrite of the same key. AGENTS.md is not written here.

//! Auto-approve because the forked dream runs off the hot path, and a
//! per-call gate would queue approvals no one answers.

use std::sync::Arc;

use houyicoder_api::memory::MemoryProvider;
use houyicoder_async::PFut;
use houyicoder_context::{MemoryError, MemoryScope};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};
use crate::agent::memory::MutationLog;
use houyicoder_api::agent_event::MemoryOperation;

/// Structured scope-promote tool.
pub struct MemoryPromoteTool {
    provider: Arc<dyn MemoryProvider>,
    recorder: Option<Arc<MutationLog>>,
}

impl MemoryPromoteTool {
    /// Construct with a shared provider handle.
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self {
            provider,
            recorder: None,
        }
    }
    /// Attach a mutation recorder.
    pub(crate) fn with_recorder(mut self, recorder: Arc<MutationLog>) -> Self {
        self.recorder = Some(recorder);
        self
    }
}

impl Tool for MemoryPromoteTool {
    fn name(&self) -> &str {
        "memory_promote"
    }
    fn description(&self) -> &str {
        "Promote one stored memory from the auto scope into the project scope \
         so it outranks automatic rewrites of the same key. Idempotent."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The kebab-case key (file stem) of the memory to promote."
                }
            },
            "required": ["key"],
            "additionalProperties": false
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let provider = Arc::clone(&self.provider);
        let recorder = self.recorder.clone();
        Box::pin(async move {
            let key = input.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
                ToolError::Failed("memory_promote: 'key' must be a non-empty string".to_string())
            })?;
            match provider.promote_memory(key) {
                Ok(()) => {
                    if let Some(recorder) = &recorder {
                        recorder.record(key, MemoryOperation::Promoted, MemoryScope::Project);
                    }
                    Ok(json!({"promoted": key}))
                }
                Err(MemoryError::NotFound) => Err(ToolError::Failed(format!(
                    "memory_promote: no memory with key '{key}'"
                ))),
                Err(e) => Err(ToolError::Failed(format!("memory_promote: {e}"))),
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

/// Structured scope-demote tool.
pub struct MemoryDemoteTool {
    provider: Arc<dyn MemoryProvider>,
    recorder: Option<Arc<MutationLog>>,
}

impl MemoryDemoteTool {
    /// Construct with a shared provider handle.
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self {
            provider,
            recorder: None,
        }
    }
    /// Attach a mutation recorder.
    pub(crate) fn with_recorder(mut self, recorder: Arc<MutationLog>) -> Self {
        self.recorder = Some(recorder);
        self
    }
}

impl Tool for MemoryDemoteTool {
    fn name(&self) -> &str {
        "memory_demote"
    }
    fn description(&self) -> &str {
        "Demote one stored memory from the project scope back into the auto \
         scope so it becomes recall-on-demand only. Idempotent."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The kebab-case key (file stem) of the memory to demote."
                }
            },
            "required": ["key"],
            "additionalProperties": false
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let provider = Arc::clone(&self.provider);
        let recorder = self.recorder.clone();
        Box::pin(async move {
            let key = input.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
                ToolError::Failed("memory_demote: 'key' must be a non-empty string".to_string())
            })?;
            match provider.demote_memory(key) {
                Ok(()) => {
                    if let Some(recorder) = &recorder {
                        recorder.record(key, MemoryOperation::Demoted, MemoryScope::Auto);
                    }
                    Ok(json!({"demoted": key}))
                }
                Err(MemoryError::NotFound) => Err(ToolError::Failed(format!(
                    "memory_demote: no memory with key '{key}'"
                ))),
                Err(e) => Err(ToolError::Failed(format!("memory_demote: {e}"))),
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
