//! The structured memory-read tool the forked consolidation agent uses to
//! inspect a topic body before merging or pruning it. The agent emits a
//! structured call with a key; the tool routes it through the memory
//! provider show_memory method, which owns the read path. The tool holds no
//! path logic of its own — the provider owns every path — so there is no
//! path-escape surface for the agent to probe. This is the
//! structurally-safe counterpart to a raw sandboxed Read over the memory
//! directory: the capability is read one memory entry, not read an
//! arbitrary file under the memory dir, and the result is the parsed
//! structured entry rather than raw file text.
//!
//! Auto-approve by construction: the tool is read-only (it changes no
//! state), so the approval gate stays off. The forked consolidation agent
//! runs autonomously off the hot path; a per-call approval gate would queue
//! approvals no one answers. Safety comes from the structured read-only
//! capability, not a human checkpoint.
//!
//! The provider is shared (Arc) with the runner that owns it, so the forked
//! dream reads through the same provider as the main runner — no second
//! handle, no divergent state.

use std::sync::Arc;

use houyicoder_api::memory::MemoryProvider;
use houyicoder_async::PFut;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};

/// A structured memory-read tool. The forked consolidation agent calls it to
/// read the full body of one topic before deciding to merge, update, or
/// delete it; the provider owns the read path.
pub struct ShowMemoryTool {
    provider: Arc<dyn MemoryProvider>,
}

impl ShowMemoryTool {
    /// Construct with a shared provider handle. The provider is shared with
    /// the runner memory so the forked dream reads the same store.
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self { provider }
    }
}

impl Tool for ShowMemoryTool {
    fn name(&self) -> &str {
        "show_memory"
    }
    fn description(&self) -> &str {
        "Read the full body of one stored memory by its key, to decide whether \
         to merge, update, or delete it. Returns the key, description, source, \
         mtime, and content. Use when the listing flagged a candidate you need \
         to inspect before acting."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The kebab-case key (file stem) of the memory to read."
                }
            },
            "required": ["key"],
            "additionalProperties": false
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let provider = Arc::clone(&self.provider);
        Box::pin(async move {
            let key = input.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
                ToolError::Failed("show_memory: 'key' must be a non-empty string".to_string())
            })?;
            match provider.show_memory(key) {
                Some(entry) => Ok(json!({
                    "key": entry.key,
                    "description": entry.description,
                    "source": entry.source.as_label(),
                    "mtime": entry.mtime_secs,
                    "content": entry.content,
                })),
                None => Err(ToolError::Failed(format!(
                    "show_memory: no memory with key '{key}'"
                ))),
            }
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
    /// Auto-approve: the tool is read-only, so there is no hard-to-reverse
    /// outward effect to gate. The forked consolidation agent runs
    /// autonomously; a per-call gate would queue approvals no one answers.
    fn requires_approval(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{MemoryEntry, MemorySource};
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// An in-memory provider that holds one entry the tool reads back.
    struct OneEntryMemory {
        entry: Mutex<Option<MemoryEntry>>,
    }
    impl MemoryProvider for OneEntryMemory {
        fn recall(&self, _q: &str, _b: usize, _surfaced: &HashSet<String>) -> Vec<MemoryEntry> {
            Vec::new()
        }
        fn add(&self, _e: MemoryEntry) -> Result<(), houyicoder_context::MemoryError> {
            Ok(())
        }
        fn show_memory(&self, key: &str) -> Option<MemoryEntry> {
            self.entry
                .lock()
                .expect("entry")
                .as_ref()
                .filter(|e| e.key == key)
                .cloned()
        }
    }

    fn entry() -> MemoryEntry {
        MemoryEntry::new(
            "user-prefers-terse",
            "User prefers terse responses",
            MemorySource::Feedback,
        )
        .with_meta("User prefers terse responses".to_string(), 123)
    }

    async fn run(tool: &ShowMemoryTool, input: Value) -> Result<Value, ToolError> {
        tool.execute(ToolCtx::new("test"), input).await
    }

    #[tokio::test]
    async fn test_memory_returns_structured_entry() {
        let p = Arc::new(OneEntryMemory {
            entry: Mutex::new(Some(entry())),
        });
        let tool = ShowMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
        let out = run(&tool, json!({"key": "user-prefers-terse"}))
            .await
            .expect("read succeeds");
        assert_eq!(out["key"], "user-prefers-terse");
        assert_eq!(out["source"], "feedback");
        assert_eq!(out["mtime"], 123);
        assert!(out["content"].is_string(), "content returned as string");
    }

    #[tokio::test]
    async fn test_memory_missing_key_errors() {
        let p = Arc::new(OneEntryMemory {
            entry: Mutex::new(Some(entry())),
        });
        let tool = ShowMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
        let err = run(&tool, json!({"key": "absent"}))
            .await
            .expect_err("absent key errors");
        assert!(
            err.to_string().contains("no memory with key 'absent'"),
            "error names the missing key: {err}"
        );
    }

    #[tokio::test]
    async fn test_memory_rejects_missing_field() {
        let p = Arc::new(OneEntryMemory {
            entry: Mutex::new(Some(entry())),
        });
        let tool = ShowMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
        let err = run(&tool, json!({}))
            .await
            .expect_err("missing key rejected");
        assert!(
            err.to_string().contains("'key'"),
            "error names the missing field: {err}"
        );
    }

    /// The structured capability surface: no path field, exactly one field.
    #[test]
    fn test_memory_schema_pins_fields() {
        let p = Arc::new(OneEntryMemory {
            entry: Mutex::new(Some(entry())),
        });
        let tool = ShowMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
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
    fn test_show_memory_auto_approves() {
        let p = Arc::new(OneEntryMemory {
            entry: Mutex::new(Some(entry())),
        });
        let tool = ShowMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
        assert!(tool.is_read_only(), "read-only");
        assert!(!tool.is_destructive(), "not destructive");
        assert!(!tool.requires_approval(), "auto-approve");
    }
}
