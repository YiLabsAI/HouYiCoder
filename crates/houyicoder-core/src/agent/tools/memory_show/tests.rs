//! Tests for this module.

use super::*;
use houyicoder_context::{MemoryEntry, MemoryError, MemorySource};
use std::sync::Mutex;

/// An in-memory provider that holds one entry the tool reads back.
struct OneEntryMemory {
    entry: Mutex<Option<MemoryEntry>>,
}
impl MemoryProvider for OneEntryMemory {
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
    fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
        Ok(())
    }
    fn show_memory(&self, key: &str) -> Option<MemoryEntry> {
        (key == "rule-one").then(|| MemoryEntry::new(key, "the only copy", MemorySource::Feedback))
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
