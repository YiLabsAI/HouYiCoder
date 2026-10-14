//! Tests for this module.

use super::*;
use houyicoder_context::MemoryEntry;
use std::sync::Mutex;

/// A recording provider that records deletes so the test asserts the
/// structured call reached delete_memory with the right key.
struct RecordingMemory {
    deleted: Mutex<Vec<String>>,
}
impl MemoryProvider for RecordingMemory {
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

/// A threaded recorder bumps once per successful delete so the dream can
/// fire one memory-saved notice per pass. A NotFound does not bump it.
#[tokio::test]
async fn test_delete_memory_counts_deletes() {
    let p = provider();
    let recorder = Arc::new(MutationLog::new());
    let tool = DeleteMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>)
        .with_recorder(recorder.clone());
    run(&tool, json!({"key": "a"})).await.expect("delete a");
    run(&tool, json!({"key": "b"})).await.expect("delete b");
    let changes = recorder.take();
    assert_eq!(changes.len(), 2);
    assert_eq!(
        changes[0].scope,
        MemoryScope::Auto,
        "the unscoped delete form removes from the auto root"
    );
}

/// A NotFound does not bump the recorder (no memory was touched).
#[tokio::test]
async fn test_memory_skips_missing_count() {
    struct EmptyMemory;
    impl MemoryProvider for EmptyMemory {
        fn add(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
            Ok(())
        }
        fn delete_memory(&self, _key: &str) -> Result<(), MemoryError> {
            Err(MemoryError::NotFound)
        }
    }
    let recorder = Arc::new(MutationLog::new());
    let tool = DeleteMemoryTool::new(Arc::new(EmptyMemory) as Arc<dyn MemoryProvider>)
        .with_recorder(recorder.clone());
    let _err = run(&tool, json!({"key": "absent"})).await;
    assert!(recorder.take().is_empty());
}

#[tokio::test]
async fn test_memory_surfaces_not_found() {
    struct EmptyMemory;
    impl MemoryProvider for EmptyMemory {
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
