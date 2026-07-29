//! The structured memory-delete tool the forked consolidation agent uses to
//! prune stale or contradicted entries. The agent emits a structured call
//! with a key; the tool routes it through the memory provider delete_memory
//! method, which owns the topic-file removal and the derived-index
//! regeneration. The tool holds no path logic of its own — the provider
//! owns every path — so there is no path-escape surface for the agent to
//! probe. This is the structurally-safe counterpart to a raw sandboxed
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
use houyicoder_context::MemoryError;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};

/// A structured memory-delete tool. The forked consolidation agent calls it
/// to prune a stale or contradicted entry; the provider owns the removal
/// and the index regeneration.
pub struct DeleteMemoryTool {
    provider: Arc<dyn MemoryProvider>,
    /// Optional write counter the caller threads in to learn how many
    /// deletions landed this pass. Incremented on a successful delete so the
    /// dream can fire one memory-saved notice per pass. None for the main
    /// runner's tool, which does not notify.
    counter: Option<Arc<std::sync::atomic::AtomicU32>>,
}

impl DeleteMemoryTool {
    /// Construct with a shared provider handle. The provider is shared with
    /// the runner memory so the forked dream deletes under the same lock.
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self {
            provider,
            counter: None,
        }
    }

    /// Thread a write counter so a successful delete bumps it. The dream
    /// shares one counter across the add + delete tools so a touch (add or
    /// delete) counts toward the notice.
    pub fn with_counter(mut self, counter: Arc<std::sync::atomic::AtomicU32>) -> Self {
        self.counter = Some(counter);
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
        let counter = self.counter.clone();
        Box::pin(async move {
            let key = input.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
                ToolError::Failed("delete_memory: 'key' must be a non-empty string".to_string())
            })?;
            match provider.delete_memory(key) {
                Ok(()) => {
                    if let Some(c) = &counter {
                        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
mod tests {
    use super::*;
    use houyicoder_context::MemoryEntry;
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// A recording provider that records deletes so the test asserts the
    /// structured call reached delete_memory with the right key.
    struct RecordingMemory {
        deleted: Mutex<Vec<String>>,
    }
    impl MemoryProvider for RecordingMemory {
        fn recall(&self, _q: &str, _b: usize, _surfaced: &HashSet<String>) -> Vec<MemoryEntry> {
            Vec::new()
        }
        fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
            Ok(())
        }
        fn delete_memory(&self, key: &str) -> Result<(), MemoryError> {
            self.deleted.lock().expect("deleted").push(key.to_string());
            Ok(())
        }
    }

    fn provider() -> Arc<RecordingMemory> {
        Arc::new(RecordingMemory {
            deleted: Mutex::new(Vec::new()),
        })
    }

    async fn run(tool: &DeleteMemoryTool, input: Value) -> Result<Value, ToolError> {
        tool.execute(ToolCtx::new("test"), input).await
    }

    #[tokio::test]
    async fn test_memory_routes_through_provider() {
        let p = provider();
        let tool = DeleteMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
        let out = run(&tool, json!({"key": "stale-thing"}))
            .await
            .expect("delete succeeds");
        assert_eq!(out, json!({"deleted": "stale-thing"}));
        assert_eq!(
            p.deleted.lock().expect("deleted").len(),
            1,
            "exactly one delete landed"
        );
        assert_eq!(p.deleted.lock().expect("deleted")[0], "stale-thing");
    }

    /// A threaded counter bumps once per successful delete so the dream can
    /// fire one memory-saved notice per pass. A NotFound does not bump it.
    #[tokio::test]
    async fn test_delete_memory_counts_deletes() {
        let p = provider();
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let tool = DeleteMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>)
            .with_counter(counter.clone());
        run(&tool, json!({"key": "a"})).await.expect("delete a");
        run(&tool, json!({"key": "b"})).await.expect("delete b");
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "two successful deletes bump the counter twice"
        );
    }

    /// A NotFound does not bump the counter (no memory was touched).
    #[tokio::test]
    async fn test_memory_skips_missing_count() {
        struct EmptyMemory;
        impl MemoryProvider for EmptyMemory {
            fn recall(&self, _q: &str, _b: usize, _surfaced: &HashSet<String>) -> Vec<MemoryEntry> {
                Vec::new()
            }
            fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
                Ok(())
            }
            fn delete_memory(&self, _key: &str) -> Result<(), MemoryError> {
                Err(MemoryError::NotFound)
            }
        }
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let tool = DeleteMemoryTool::new(Arc::new(EmptyMemory) as Arc<dyn MemoryProvider>)
            .with_counter(counter.clone());
        let _err = run(&tool, json!({"key": "absent"})).await;
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a failed delete does not bump the counter"
        );
    }

    #[tokio::test]
    async fn test_memory_surfaces_not_found() {
        struct EmptyMemory;
        impl MemoryProvider for EmptyMemory {
            fn recall(&self, _q: &str, _b: usize, _surfaced: &HashSet<String>) -> Vec<MemoryEntry> {
                Vec::new()
            }
            fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
                Ok(())
            }
            fn delete_memory(&self, _key: &str) -> Result<(), MemoryError> {
                Err(MemoryError::NotFound)
            }
        }
        let tool = DeleteMemoryTool::new(Arc::new(EmptyMemory) as Arc<dyn MemoryProvider>);
        let err = run(&tool, json!({"key": "absent"}))
            .await
            .expect_err("not found errors");
        assert!(
            err.to_string().contains("no memory with key 'absent'"),
            "error names the missing key: {err}"
        );
    }

    #[tokio::test]
    async fn test_memory_rejects_missing_field() {
        let p = provider();
        let tool = DeleteMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
        let err = run(&tool, json!({}))
            .await
            .expect_err("missing key rejected");
        assert!(
            err.to_string().contains("'key'"),
            "error names the missing field: {err}"
        );
        assert!(
            p.deleted.lock().expect("deleted").is_empty(),
            "no delete landed on a rejected call"
        );
    }

    /// The structured capability surface: no path field, exactly one field.
    #[test]
    fn test_memory_schema_pins_fields() {
        let p = provider();
        let tool = DeleteMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
        let schema = tool.input_schema();
        let props = schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("properties object");
        assert!(
            !props.contains_key("path"),
            "no path field — the provider owns paths"
        );
        assert_eq!(props.len(), 1, "exactly one field (key)");
    }

    #[test]
    fn test_memory_destructive_auto_approves() {
        let p = provider();
        let tool = DeleteMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
        assert!(!tool.is_read_only(), "a delete mutates the store");
        assert!(tool.is_destructive(), "destructive");
        assert!(!tool.requires_approval(), "auto-approve");
    }
}
