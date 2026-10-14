//! The structured memory scope-flow tools the forked consolidation dream
//! uses to move a topic between the auto and project storage scopes. The
//! dream emits a structured call with a key; the tool routes it through the
//! memory provider promote_memory or demote_memory method, which owns the
//! file-op sequence: merge or strip the rule sentence in the project memory
//! file (the always-on carrier), move the topic file between the auto and
//! project roots, and regenerate the derived indexes. The tool holds no
//! path logic of its own — the provider owns every path — so there is no
//! path-escape surface for the agent to probe.
//!
//! Destructive-but-reversible (a promote moves a topic, a demote moves it
//! back; the rule sentence in the carrier file is append-only on promote
//! and line-removal on demote) and auto-approve: the forked dream runs
//! autonomously off the hot path; a per-call approval gate would queue
//! approvals no one answers and starve scope flow. Safety comes from the
//! structured capability plus the dream prompt's promote-when / demote-when
//! guidance, not from a human checkpoint here.
//!
//! The provider is shared (Arc) with the runner that owns it, so the
//! forked dream mutates the same store the main runner reads.

use std::sync::Arc;

use houyicoder_api::memory::MemoryProvider;
use houyicoder_async::PFut;
use houyicoder_context::{MemoryError, MemoryScope};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};
use crate::agent::memory::MutationLog;
use houyicoder_api::agent_event::MemoryOperation;

/// A structured scope-promote tool. The forked consolidation dream calls it
/// when a rule has crossed the promotion threshold (high recall frequency,
/// or repeated gate violations): the topic moves from the auto scope into
/// the project scope, and the rule sentence lands in the project memory
/// file so the rule is always-on rather than recall-on-demand.
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
        "Promote one stored memory from the auto scope into the project scope \
         (the always-on carrier). Use for rules that have crossed the \
         promotion threshold: high recall frequency, or repeated PreToolUse \
         gate violations on that rule. The operation merges the rule \
         sentence (the first content line) into the project memory file so \
         the rule is always-on rather than recall-on-demand, and moves the \
         topic file into the project memory dir so recall still finds it. \
         Idempotent: a topic already in the project scope only refreshes the \
         carrier line."
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
/// when an always-on rule has decayed (long unstirred + no gate violations):
/// the rule sentence leaves the project memory file (freeing always-on
/// prefix budget) and the topic file moves back into the auto scope so the
/// topic is recall-on-demand only. The reverse of promote_memory.
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
         scope. Use for rules that have decayed: long unstirred in recall, \
         no recent gate violations. The operation removes the rule sentence \
         from the project memory file (freeing always-on prefix budget) \
         and moves the topic file back into the auto memory dir so the \
         topic is recall-on-demand only. Idempotent: a topic already in \
         the auto scope only strips the carrier line."
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
