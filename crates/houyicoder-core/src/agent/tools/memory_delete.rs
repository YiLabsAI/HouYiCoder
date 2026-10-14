//! The structured memory-delete tool the forked consolidation agent uses to
//! prune stale or contradicted entries. The agent emits a structured call
//! with a key; the tool routes it through the memory provider delete_memory
//! method, which owns the topic-file removal and the derived-index
//! regeneration. The tool holds no path logic of its own — the provider
//! owns every path — so there is no path-escape surface for the agent to
//! probe. This is the structurally-safe recorderpart to a raw sandboxed
//! file delete over the memory directory: the capability is delete one
//! memory entry by key, not delete an arbitrary file under the memory dir.
//!
//! Destructive but auto-approve: a delete removes one topic, which is
//! reversible by re-saving the same key (the dream can save_memory it
//! back), and the dream runs off the hot path bounded by the prompt's
//! what-to-prune guidance plus the forked maxTurns backstop. A per-call
//! approval gate would queue approvals no one answers and starve
//! consolidation — the forked agent runs autonomously. Safety comes from
//! the structured capability (the provider owns paths, so no
//! path-escape; the key is validator-checked) plus the prompt guidance,
//! not from a human checkpoint here.
//!
//! The provider is shared (Arc) with the runner that owns it, so the
//! forked dream deletes through the same write lock as the main runner —
//! no cross-write orphan within the process.

use std::sync::Arc;

use houyicoder_api::memory::MemoryProvider;
use houyicoder_async::PFut;
use houyicoder_context::{MemoryError, MemoryScope};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};
use crate::agent::memory::MutationLog;
use houyicoder_api::agent_event::MemoryOperation;

/// A structured memory-delete tool. The forked consolidation agent calls it
/// to prune a stale or contradicted entry; the provider owns the removal
/// and the index regeneration.
pub struct DeleteMemoryTool {
    provider: Arc<dyn MemoryProvider>,
    /// Optional write recorder the caller threads in to learn how many
    /// deletions landed this pass. Incremented on a successful delete so the
    /// dream can fire one memory-saved notice per pass. None for the main
    /// runner's tool, which does not notify.
    recorder: Option<Arc<MutationLog>>,
}

impl DeleteMemoryTool {
    /// Construct with a shared provider handle. The provider is shared with
    /// the runner memory so the forked dream deletes under the same lock.
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self {
            provider,
            recorder: None,
        }
    }

    /// Thread a write recorder so a successful delete bumps it. The dream
    /// shares one recorder across the add + delete tools so a touch (add or
    /// delete) counts toward the notice.
    pub(crate) fn with_recorder(mut self, recorder: Arc<MutationLog>) -> Self {
        self.recorder = Some(recorder);
        self
    }
}

impl Tool for DeleteMemoryTool {
    fn name(&self) -> &str {
        "delete_memory"
    }
    fn description(&self) -> &str {
        "Delete one stored memory by key. Use to prune entries that are stale, \
         contradicted by the current code or project state, or superseded by a \
         merged successor. Deletion removes the topic file; the index \
         regenerates after the dream so the pointer disappears. Deletion is \
         reversible by saving the same key again."
    }
    fn input_schema(&self) -> Value {
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
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let provider = Arc::clone(&self.provider);
        let recorder = self.recorder.clone();
        Box::pin(async move {
            let key = input.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
                ToolError::Failed("delete_memory: 'key' must be a non-empty string".to_string())
            })?;
            match provider.delete_memory(key) {
                Ok(()) => {
                    if let Some(recorder) = &recorder {
                        // This tool deletes through the unscoped form, whose
                        // contract is the auto root (the dream's consolidation
                        // target), so the notice names auto.
                        recorder.record(key, MemoryOperation::Deleted, MemoryScope::Auto);
                    }
                    Ok(json!({"deleted": key}))
                }
                Err(MemoryError::NotFound) => Err(ToolError::Failed(format!(
                    "delete_memory: no memory with key '{key}'"
                ))),
                Err(e) => Err(ToolError::Failed(format!("delete_memory: {e}"))),
            }
        })
    }
    fn is_read_only(&self) -> bool {
        false
    }
    /// Destructive: a delete removes one topic file. Auto-approve (see the
    /// module doc) — the forked agent runs autonomously off the hot path,
    /// bounded by the prompt guidance plus the forked maxTurns backstop.
    fn is_destructive(&self) -> bool {
        true
    }
    fn requires_approval(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests;
