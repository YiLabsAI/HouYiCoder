//! Tests for this module.

use super::*;
use houyicoder_context::MemoryEntry;
use std::sync::Mutex;

/// A recording provider that records promote / demote calls so the tool
/// test asserts the structured call reached the provider with the key.
struct RecordingMemory {
    promoted: Mutex<Vec<String>>,
    demoted: Mutex<Vec<String>>,
}
impl MemoryProvider for RecordingMemory {
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

/// A threaded recorder bumps once per successful promote / demote so the
/// dream can fire one memory-saved notice per pass that touches the
/// store. A NotFound does not bump it.
#[tokio::test]
async fn test_promote_demote_share_recorder() {
    let p = provider();
    let recorder = Arc::new(MutationLog::new());
    let promote = PromoteMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>)
        .with_recorder(recorder.clone());
    let demote = DemoteMemoryTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>)
        .with_recorder(recorder.clone());
    run_promote(&promote, json!({"key": "k1"}))
        .await
        .expect("promote");
    run_demote(&demote, json!({"key": "k2"}))
        .await
        .expect("demote");
    let changes = recorder.take();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0].key, "k1");
    assert_eq!(changes[0].operation, MemoryOperation::Promoted);
    assert_eq!(
        changes[0].scope,
        MemoryScope::Project,
        "a promote lands the topic in the project root"
    );
    assert_eq!(changes[1].key, "k2");
    assert_eq!(changes[1].operation, MemoryOperation::Demoted);
    assert_eq!(
        changes[1].scope,
        MemoryScope::Auto,
        "a demote lands the topic back in the auto root"
    );
}

/// A missing key is surfaced as a NotFound error the model can act on
/// (the rule sentence is not there to promote / demote).
#[tokio::test]
async fn test_promote_missing_key_errors() {
    // A provider whose promote_memory returns NotFound.
    struct NotFoundProvider;
    impl MemoryProvider for NotFoundProvider {
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
