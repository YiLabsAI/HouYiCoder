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
use houyicoder_context::MemoryError;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};

/// A structured scope-promote tool. The forked consolidation dream calls it
/// when a rule has crossed the promotion threshold (high recall frequency,
/// or repeated gate violations): the topic moves from the auto scope into
/// the project scope, and the rule sentence lands in the project memory
/// file so the rule is always-on rather than recall-on-demand.
pub struct PromoteMemoryTool {
    provider: Arc<dyn MemoryProvider>,
    /// Optional write counter the caller threads in to learn how many
    /// scope-flow ops landed this pass. Shared with the add + delete tools
    /// so one notice fires per pass that touches the store. None for the
    /// main runner's tool, which does not notify.
    counter: Option<Arc<std::sync::atomic::AtomicU32>>,
}

impl PromoteMemoryTool {
    /// Construct with a shared provider handle. The provider is shared with
    /// the runner memory so the forked dream promotes under the same lock.
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self {
            provider,
            counter: None,
        }
    }
    /// Thread a write counter so a successful promote bumps it. The dream
    /// shares one counter across the add + delete + promote + demote tools so
    /// any touch counts toward the notice.
    pub fn with_counter(mut self, counter: Arc<std::sync::atomic::AtomicU32>) -> Self {
        self.counter = Some(counter);
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
        let counter = self.counter.clone();
        Box::pin(async move {
            let key = input.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
                ToolError::Failed("promote_memory: 'key' must be a non-empty string".to_string())
            })?;
            match provider.promote_memory(key) {
                Ok(()) => {
                    if let Some(c) = &counter {
                        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    counter: Option<Arc<std::sync::atomic::AtomicU32>>,
}

impl DemoteMemoryTool {
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self {
            provider,
            counter: None,
        }
    }
    pub fn with_counter(mut self, counter: Arc<std::sync::atomic::AtomicU32>) -> Self {
        self.counter = Some(counter);
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
        let counter = self.counter.clone();
        Box::pin(async move {
            let key = input.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
                ToolError::Failed("demote_memory: 'key' must be a non-empty string".to_string())
            })?;
            match provider.demote_memory(key) {
                Ok(()) => {
                    if let Some(c) = &counter {
                        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
mod tests {
    use super::*;
    use houyicoder_context::MemoryEntry;
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// A recording provider that records promote / demote calls so the tool
    /// test asserts the structured call reached the provider with the key.
    struct RecordingMemory {
        promoted: Mutex<Vec<String>>,
        demoted: Mutex<Vec<String>>,
    }
    impl MemoryProvider for RecordingMemory {
        fn recall(&self, _q: &str, _b: usize, _surfaced: &HashSet<String>) -> Vec<MemoryEntry> {
            Vec::new()
        }
        fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
            Ok(())
        }
        fn promote_memory(&self, key: &str) -> Result<(), MemoryError> {
            self.promoted
                .lock()
                .expect("promoted")
                .push(key.to_string());
            Ok(())
        }
        fn demote_memory(&self, key: &str) -> Result<(), MemoryError> {
            self.demoted.lock().expect("demoted").push(key.to_string());
            Ok(())
        }
    }

    fn provider() -> Arc<RecordingMemory> {
        Arc::new(RecordingMemory {
            promoted: Mutex::new(Vec::new()),
            demoted: Mutex::new(Vec::new()),
        })
    }

    async fn run_promote(tool: &PromoteMemoryTool, input: Value) -> Result<Value, ToolError> {
        tool.execute(ToolCtx::new("test"), input).await
    }
    async fn run_demote(tool: &DemoteMemoryTool, input: Value) -> Result<Value, ToolError> {
        tool.execute(ToolCtx::new("test"), input).await
    }

    #[tokio::test]
    async fn test_promote_routes_through_provider() {
        let p = provider();
        let tool = PromoteMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
        let out = run_promote(&tool, json!({"key": "test-naming"}))
            .await
            .expect("promote succeeds");
        assert_eq!(out, json!({"promoted": "test-naming"}));
        assert_eq!(p.promoted.lock().expect("promoted").len(), 1);
    }

    #[tokio::test]
    async fn test_demote_routes_through_provider() {
        let p = provider();
        let tool = DemoteMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
        let out = run_demote(&tool, json!({"key": "merge-freeze"}))
            .await
            .expect("demote succeeds");
        assert_eq!(out, json!({"demoted": "merge-freeze"}));
        assert_eq!(p.demoted.lock().expect("demoted").len(), 1);
    }

    /// A threaded counter bumps once per successful promote / demote so the
    /// dream can fire one memory-saved notice per pass that touches the
    /// store. A NotFound does not bump it.
    #[tokio::test]
    async fn test_promote_demote_share_counter() {
        let p = provider();
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let promote = PromoteMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>)
            .with_counter(counter.clone());
        let demote = DemoteMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>)
            .with_counter(counter.clone());
        run_promote(&promote, json!({"key": "k1"}))
            .await
            .expect("promote");
        run_demote(&demote, json!({"key": "k2"}))
            .await
            .expect("demote");
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "promote + demote bump the shared counter"
        );
    }

    /// A missing key is surfaced as a NotFound error the model can act on
    /// (the rule sentence is not there to promote / demote).
    #[tokio::test]
    async fn test_promote_missing_key_errors() {
        // A provider whose promote_memory returns NotFound.
        struct NotFoundProvider;
        impl MemoryProvider for NotFoundProvider {
            fn recall(&self, _q: &str, _b: usize, _s: &HashSet<String>) -> Vec<MemoryEntry> {
                Vec::new()
            }
            fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
                Ok(())
            }
            // promote_memory defaults to Err(NotFound), so no override.
        }
        let tool = PromoteMemoryTool::new(Arc::new(NotFoundProvider) as Arc<dyn MemoryProvider>);
        let err = run_promote(&tool, json!({"key": "absent"}))
            .await
            .expect_err("missing key rejected");
        assert!(
            err.to_string().contains("no memory with key 'absent'"),
            "error names the missing key"
        );
    }
}
