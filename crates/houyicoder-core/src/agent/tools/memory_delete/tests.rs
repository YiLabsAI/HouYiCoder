//! Unit tests for MemoryDeleteTool (explicit scope and pinned scope).

use super::*;
use houyicoder_context::{MemoryEntry, MemoryOrigin, MemoryType};
use std::sync::Mutex;

struct RecordingMemory {
    deleted: Mutex<Vec<(String, MemoryScope)>>,
    entries: Mutex<std::collections::HashMap<(String, &'static str), MemoryEntry>>,
}

impl MemoryProvider for RecordingMemory {
    fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
        Ok(())
    }
    fn delete_memory(&self, key: &str) -> Result<(), MemoryError> {
        self.deleted
            .lock()
            .expect("deleted")
            .push((key.to_string(), MemoryScope::Auto));
        Ok(())
    }
    fn delete_memory_in_scope(&self, key: &str, scope: MemoryScope) -> Result<(), MemoryError> {
        self.deleted
            .lock()
            .expect("deleted")
            .push((key.to_string(), scope));
        Ok(())
    }
    fn show_memory_in_scope(&self, key: &str, scope: MemoryScope) -> Option<MemoryEntry> {
        self.entries
            .lock()
            .expect("entries")
            .get(&(key.to_string(), scope.as_label()))
            .cloned()
    }
}

fn provider() -> Arc<RecordingMemory> {
    Arc::new(RecordingMemory {
        deleted: Mutex::new(Vec::new()),
        entries: Mutex::new(std::collections::HashMap::new()),
    })
}

async fn run(tool: &MemoryDeleteTool, input: Value) -> Result<Value, ToolError> {
    tool.execute(ToolCtx::new("test"), input).await
}

#[tokio::test]
async fn test_delete_requires_explicit_scope() {
    let p = provider();
    let tool = MemoryDeleteTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
    assert_eq!(tool.name(), "memory_delete");
    let missing_scope = run(&tool, json!({"key": "stale-thing"})).await;
    assert!(missing_scope.is_err(), "unpinned delete requires scope");

    let out = run(&tool, json!({"key": "stale-thing", "scope": "project"}))
        .await
        .expect("scoped delete succeeds");
    assert_eq!(out, json!({"deleted": "stale-thing", "scope": "project"}));
    assert_eq!(
        *p.deleted.lock().expect("deleted"),
        vec![("stale-thing".to_string(), MemoryScope::Project)]
    );
}

#[tokio::test]
async fn test_pinned_delete_counts_deletes() {
    let p = provider();
    let recorder = Arc::new(MutationLog::new());
    let tool =
        MemoryDeleteTool::new_pinned(Arc::clone(&p) as Arc<dyn MemoryProvider>, MemoryScope::Auto)
            .with_recorder(recorder.clone());
    run(&tool, json!({"key": "a"})).await.expect("delete a");
    run(&tool, json!({"key": "b"})).await.expect("delete b");
    let changes = recorder.take();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0].scope, MemoryScope::Auto);
}

#[tokio::test]
async fn test_delete_missing_errors() {
    struct EmptyMemory;
    impl MemoryProvider for EmptyMemory {
        fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
            Ok(())
        }
    }
    let recorder = Arc::new(MutationLog::new());
    let tool = MemoryDeleteTool::new(Arc::new(EmptyMemory) as Arc<dyn MemoryProvider>)
        .with_recorder(recorder.clone());
    assert!(!tool.is_read_only());
    assert!(tool.is_destructive());
    assert!(!tool.requires_approval());
    let props = tool.input_schema()["properties"]
        .as_object()
        .expect("props")
        .clone();
    assert_eq!(props.len(), 2);
    let err = run(&tool, json!({"key": "absent", "scope": "auto"}))
        .await
        .expect_err("not found errors");
    assert!(err.to_string().contains("no memory with key 'absent'"));
    assert!(recorder.take().is_empty());
}

/// A background consolidation writer must not delete memories authored by
/// the user or the primary agent.
#[tokio::test]
async fn test_background_delete_guard() {
    let p = provider();
    p.entries.lock().expect("entries").insert(
        ("user-fact".to_string(), MemoryScope::Auto.as_label()),
        MemoryEntry::new("user-fact", "body", MemoryType::Feedback).with_origin(MemoryOrigin::User),
    );
    p.entries.lock().expect("entries").insert(
        ("agent-fact".to_string(), MemoryScope::Auto.as_label()),
        MemoryEntry::new("agent-fact", "body", MemoryType::Feedback)
            .with_origin(MemoryOrigin::PrimaryAgent),
    );

    let dream_tool =
        MemoryDeleteTool::new_pinned(Arc::clone(&p) as Arc<dyn MemoryProvider>, MemoryScope::Auto)
            .with_origin(MemoryOrigin::Dream);

    let err_user = run(&dream_tool, json!({"key": "user-fact"}))
        .await
        .expect_err("dream cannot delete user memory");
    assert!(err_user.to_string().contains("protected memory"));

    let err_agent = run(&dream_tool, json!({"key": "agent-fact"}))
        .await
        .expect_err("dream cannot delete primary agent memory");
    assert!(err_agent.to_string().contains("protected memory"));

    assert!(p.deleted.lock().expect("deleted").is_empty());
}
