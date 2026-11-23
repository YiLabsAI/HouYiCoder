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

/// A structured scope-promote tool. The forked consolidation dream calls it
/// when a rule crosses the promotion threshold. The topic moves from the auto
/// scope into the project scope, where it outranks an automatic rewrite of
/// the same key.
pub struct PromoteMemoryTool {
    provider: Arc<dyn MemoryProvider>,
    /// Optional write recorder the caller threads in to learn how many
    /// scope-flow ops landed this pass. Shared with the add + delete tools
    /// so one notice fires per pass that touches the store. None for the
    /// main runner's tool, which does not notify.
    recorder: Option<Arc<MutationLog>>,
}

impl PromoteMemoryTool {
    /// Construct with a shared provider handle. The provider is shared with
    /// the runner memory so the forked dream promotes under the same lock.
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self {
            provider,
            recorder: None,
        }
    }
    /// Thread a write recorder so a successful promote bumps it. The dream
    /// shares one recorder across the add + delete + promote + demote tools so
    /// any touch counts toward the notice.
    pub(crate) fn with_recorder(mut self, recorder: Arc<MutationLog>) -> Self {
        self.recorder = Some(recorder);
        self
    }
}

impl Tool for PromoteMemoryTool {
    fn name(&self) -> &str {
        "promote_memory"
    }
    fn description(&self) -> &str {
        "Promote one stored memory from the auto scope into the project scope. \
         Use for rules with high recall frequency or repeated PreToolUse gate \
         violations. The topic moves into the project memory dir, where the \
         project scope outranks the automatic one on a key collision, so the \
         rule survives an automatic rewrite of the same key. Idempotent."
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
                ToolError::Failed("promote_memory: 'key' must be a non-empty string".to_string())
            })?;
            match provider.promote_memory(key) {
                Ok(()) => {
                    if let Some(recorder) = &recorder {
                        // A promote moves the topic out of the auto root into
                        // the project root, so the notice names project.
                        recorder.record(key, MemoryOperation::Promoted, MemoryScope::Project);
                    }
                    Ok(json!({"promoted": key}))
                }
                Err(MemoryError::NotFound) => Err(ToolError::Failed(format!(
                    "promote_memory: no memory with key '{key}'"
                ))),
                Err(e) => Err(ToolError::Failed(format!("promote_memory: {e}"))),
            }
        })
    }
    fn is_read_only(&self) -> bool {
        false
    }
    /// Not destructive: a promote moves a topic + appends a carrier line;
    /// reversible by demote_memory. No hard-to-reverse outward effect.
    fn is_destructive(&self) -> bool {
        false
    }
    fn requires_approval(&self) -> bool {
        false
    }
}

/// A structured scope-demote tool. The forked consolidation dream calls it
/// when a project rule has decayed and is long unstirred with no gate
/// violations. The topic moves back into the auto scope so it is
/// recall-on-demand only. The reverse of promote_memory.
pub struct DemoteMemoryTool {
    provider: Arc<dyn MemoryProvider>,
    recorder: Option<Arc<MutationLog>>,
}

impl DemoteMemoryTool {
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self {
            provider,
            recorder: None,
        }
    }
    pub(crate) fn with_recorder(mut self, recorder: Arc<MutationLog>) -> Self {
        self.recorder = Some(recorder);
        self
    }
}

impl Tool for DemoteMemoryTool {
    fn name(&self) -> &str {
        "demote_memory"
    }
    fn description(&self) -> &str {
        "Demote one stored memory from the project scope back into the auto \
         scope. Use for decayed rules that are long unstirred in recall with \
         no recent gate violations. The topic moves back into the auto memory \
         dir so it is recall-on-demand only. Idempotent."
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
                ToolError::Failed("demote_memory: 'key' must be a non-empty string".to_string())
            })?;
            match provider.demote_memory(key) {
                Ok(()) => {
                    if let Some(recorder) = &recorder {
                        // A demote moves the topic back into the auto root, so
                        // the notice names auto.
                        recorder.record(key, MemoryOperation::Demoted, MemoryScope::Auto);
                    }
                    Ok(json!({"demoted": key}))
                }
                Err(MemoryError::NotFound) => Err(ToolError::Failed(format!(
                    "demote_memory: no memory with key '{key}'"
                ))),
                Err(e) => Err(ToolError::Failed(format!("demote_memory: {e}"))),
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
