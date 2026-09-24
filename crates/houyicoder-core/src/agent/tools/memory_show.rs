//! The structured memory-read tool an agent calls to read one stored memory
//! body in full. The tool routes a key through the provider's read methods,
//! which own the read path, so there is no path-escape surface for the agent
//! to probe and the result is the parsed entry rather than raw file text. A
//! key stored in more than one scope is an error naming the scopes, never a
//! silent newest.
//!
//! Read-only by construction, so the approval gate stays off.

use std::sync::Arc;

use houyicoder_api::memory::MemoryProvider;
use houyicoder_async::PFut;
use houyicoder_context::{MemoryEntry, MemoryScope};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};

/// A structured memory-read tool. An agent calls it to read the full body of
/// one memory before deciding whether to merge, update, delete, or act on it;
/// the provider owns the read path.
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
        "Read the full body of one stored memory by its key. Returns the key, \
         description, source, mtime, and content. Use it when a memory listing \
         shows a candidate you need to inspect in full before acting on it. \
         Pass scope when the same key is stored in more than one scope."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The kebab-case key (file stem) of the memory to read."
                },
                "scope": {
                    "type": "string",
                    "enum": ["user", "project", "auto"],
                    "description": "The scope to read from, when the same key is \
                                    stored in more than one."
                }
            },
            "required": ["key"],
            "additionalProperties": false
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let provider = Arc::clone(&self.provider);
        Box::pin(async move {
            let key = input
                .get("key")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .ok_or_else(|| {
                    ToolError::InvalidInput(
                        "show_memory: 'key' must be a non-empty string".to_string(),
                    )
                })?;
            let scope = match input.get("scope") {
                Some(v) => Some(parse_scope(v)?),
                None => None,
            };
            let entry = read_body(&*provider, key, scope)?;
            Ok(json!({
                "key": entry.key,
                "description": entry.description,
                "source": entry.source.as_label(),
                "mtime": entry.mtime_secs,
                "content": entry.content,
            }))
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

/// Parse the optional scope argument. An unrecognized label is a caller error
/// rather than a silent default, so a typo cannot read from a scope the caller
/// did not choose.
fn parse_scope(value: &Value) -> Result<MemoryScope, ToolError> {
    let label = value.as_str().ok_or_else(|| {
        ToolError::InvalidInput("show_memory: 'scope' must be a string".to_string())
    })?;
    MemoryScope::from_label(label).ok_or_else(|| {
        ToolError::InvalidInput(format!(
            "show_memory: unknown scope '{label}'; use user, project, or auto"
        ))
    })
}

/// Read the body for a key, resolving the scope question first. A key stored
/// in one scope reads straight through. A key stored in more than one has two
/// different bodies, so the caller must name the scope it wants — picking the
/// newest would hand back a body the caller did not ask for and cannot tell
/// apart from the one it did.
fn read_body(
    provider: &dyn MemoryProvider,
    key: &str,
    scope: Option<MemoryScope>,
) -> Result<MemoryEntry, ToolError> {
    if let Some(scope) = scope {
        return provider.show_memory_in_scope(key, scope).ok_or_else(|| {
            ToolError::Failed(format!(
                "show_memory: no memory with key '{key}' in scope '{}'",
                scope.as_label()
            ))
        });
    }
    let scopes = provider.scopes_for_key(key);
    if scopes.len() > 1 {
        let names: Vec<&str> = scopes.iter().map(|s| s.as_label()).collect();
        return Err(ToolError::InvalidInput(format!(
            "show_memory: '{key}' is stored in {} scopes ({}); pass scope to choose one",
            scopes.len(),
            names.join(", ")
        )));
    }
    provider
        .show_memory(key)
        .ok_or_else(|| ToolError::Failed(format!("show_memory: no memory with key '{key}'")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{MemoryEntry, MemoryError, MemorySource};
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
        fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
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

    /// A provider holding one key in two scopes — the shape a store keeps when
    /// an explicit copy and an auto copy share a name. Each scope answers with
    /// its own body, and the merged read answers with the auto one.
    struct TwoScopeMemory;

    impl MemoryProvider for TwoScopeMemory {
        fn recall(&self, _q: &str, _b: usize, _s: &HashSet<String>) -> Vec<MemoryEntry> {
            Vec::new()
        }
        fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
            Ok(())
        }
        fn show_memory(&self, key: &str) -> Option<MemoryEntry> {
            self.show_memory_in_scope(key, MemoryScope::Auto)
        }
        fn show_memory_in_scope(&self, key: &str, scope: MemoryScope) -> Option<MemoryEntry> {
            let scoped = matches!(scope, MemoryScope::Project | MemoryScope::Auto);
            (key == "rule-x" && scoped).then(|| {
                MemoryEntry::new(
                    key,
                    format!("body stored in {}", scope.as_label()),
                    MemorySource::Feedback,
                )
            })
        }
        fn scopes_for_key(&self, key: &str) -> Vec<MemoryScope> {
            if key == "rule-x" {
                vec![MemoryScope::Project, MemoryScope::Auto]
            } else {
                Vec::new()
            }
        }
    }

    /// A provider holding a key in exactly one scope — an unscoped read of it
    /// must go straight through to the merged read rather than refuse.
    struct SingleScopeMemory;

    impl MemoryProvider for SingleScopeMemory {
        fn recall(&self, _q: &str, _b: usize, _s: &HashSet<String>) -> Vec<MemoryEntry> {
            Vec::new()
        }
        fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
            Ok(())
        }
        fn show_memory(&self, key: &str) -> Option<MemoryEntry> {
            (key == "rule-one")
                .then(|| MemoryEntry::new(key, "the only copy", MemorySource::Feedback))
        }
        fn scopes_for_key(&self, key: &str) -> Vec<MemoryScope> {
            if key == "rule-one" {
                vec![MemoryScope::User]
            } else {
                Vec::new()
            }
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
            matches!(err, ToolError::InvalidInput(_)),
            "a malformed call is a caller error: {err:?}"
        );
        assert!(
            err.to_string().contains("'key'"),
            "error names the missing field: {err}"
        );
    }

    /// The structured capability surface: no path field, key and an optional
    /// scope. There is no third field, so the agent cannot ask for a raw file.
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
        assert_eq!(props.len(), 2, "key and scope, nothing else");
        assert!(props.contains_key("key") && props.contains_key("scope"));
    }

    /// A named scope reads that scope's copy, not whichever one is newest.
    #[tokio::test]
    async fn test_memory_scope_selects_copy() {
        let tool = ShowMemoryTool::new(Arc::new(TwoScopeMemory) as Arc<dyn MemoryProvider>);
        let out = run(&tool, json!({"key": "rule-x", "scope": "project"}))
            .await
            .expect("scoped read");
        assert_eq!(out["content"], "body stored in project");
        let out = run(&tool, json!({"key": "rule-x", "scope": "auto"}))
            .await
            .expect("scoped read");
        assert_eq!(out["content"], "body stored in auto");
    }

    /// A key in two scopes with no scope named is an error listing them, so a
    /// reader cannot receive a body it did not ask for and cannot tell apart
    /// from the one it did.
    #[tokio::test]
    async fn test_memory_ambiguous_key_errors() {
        let tool = ShowMemoryTool::new(Arc::new(TwoScopeMemory) as Arc<dyn MemoryProvider>);
        let err = run(&tool, json!({"key": "rule-x"}))
            .await
            .expect_err("ambiguous key errors");
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "ambiguity is a caller error: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("project") && msg.contains("auto"),
            "error lists the scopes to choose from: {msg}"
        );
        assert!(msg.contains("pass scope"), "error names the way out: {msg}");
    }

    /// A named scope that does not hold the key reports that scope, with no
    /// fallback to another root that would answer with a different body.
    #[tokio::test]
    async fn test_memory_scope_absent_key() {
        let tool = ShowMemoryTool::new(Arc::new(TwoScopeMemory) as Arc<dyn MemoryProvider>);
        let err = run(&tool, json!({"key": "rule-x", "scope": "user"}))
            .await
            .expect_err("absent scope errors");
        assert!(
            err.to_string().contains("in scope 'user'"),
            "error names the scope that was read: {err}"
        );
    }

    /// A scope label the provider does not know is rejected rather than
    /// silently treated as the default.
    #[tokio::test]
    async fn test_memory_unknown_scope_errors() {
        let tool = ShowMemoryTool::new(Arc::new(TwoScopeMemory) as Arc<dyn MemoryProvider>);
        let err = run(&tool, json!({"key": "rule-x", "scope": "team"}))
            .await
            .expect_err("unknown scope rejected");
        assert!(
            err.to_string().contains("unknown scope 'team'"),
            "error names the label and the vocabulary: {err}"
        );
    }

    /// A key living in exactly one scope reads without naming it — the
    /// ambiguity refusal starts at a second copy, not at the first.
    #[tokio::test]
    async fn test_memory_single_scope_reads() {
        let tool = ShowMemoryTool::new(Arc::new(SingleScopeMemory) as Arc<dyn MemoryProvider>);
        let out = run(&tool, json!({"key": "rule-one"}))
            .await
            .expect("single-scope read succeeds");
        assert_eq!(out["content"], "the only copy");
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
