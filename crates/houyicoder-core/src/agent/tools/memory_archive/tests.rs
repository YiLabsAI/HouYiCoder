//! Unit tests for MemoryArchiveTool.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use houyicoder_api::agent_event::MemoryOperation;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_context::{MemoryEntry, MemoryError, MemoryScope, MemoryType};
use serde_json::json;

use super::MemoryArchiveTool;
use crate::agent::memory::MutationLog;

#[derive(Default)]
struct StubArchiveMemory {
    active: Mutex<HashMap<(String, String), MemoryEntry>>,
    archived: Mutex<Vec<(String, MemoryScope)>>,
}

impl MemoryProvider for StubArchiveMemory {
    fn add(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        self.active
            .lock()
            .expect("lock")
            .insert((entry.key.clone(), "auto".into()), entry);
        Ok(())
    }
    fn archive_memory(&self, key: &str, scope: MemoryScope) -> Result<(), MemoryError> {
        let mut active = self.active.lock().expect("lock");
        if active
            .remove(&(key.to_string(), scope.as_label().to_string()))
            .is_some()
        {
            self.archived
                .lock()
                .expect("archived")
                .push((key.to_string(), scope));
            Ok(())
        } else {
            Err(MemoryError::NotFound)
        }
    }
}

#[tokio::test]
async fn test_archive_moves_entry() {
    let mem = Arc::new(StubArchiveMemory::default());
    mem.active.lock().expect("lock").insert(
        ("stale-rule".into(), "project".into()),
        MemoryEntry::new("stale-rule", "body", MemoryType::Project),
    );
    let log = Arc::new(MutationLog::new());
    let tool = MemoryArchiveTool::new(Arc::clone(&mem) as Arc<dyn MemoryProvider>)
        .with_recorder(Arc::clone(&log));
    assert_eq!(tool.name(), "memory_archive");

    let out = tool
        .execute(
            ToolCtx::new("c1"),
            json!({"key": "stale-rule", "scope": "project"}),
        )
        .await
        .expect("archive succeeds");
    assert_eq!(out, json!({"archived": "stale-rule", "scope": "project"}));

    let changes = log.take();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].operation, MemoryOperation::Archived);
    assert_eq!(changes[0].scope, MemoryScope::Project);

    let missing = tool
        .execute(
            ToolCtx::new("c2"),
            json!({"key": "stale-rule", "scope": "project"}),
        )
        .await;
    assert!(missing.is_err());
}

#[tokio::test]
async fn test_pinned_archive_hides_scope() {
    let mem = Arc::new(StubArchiveMemory::default());
    mem.add(MemoryEntry::new("auto-item", "body", MemoryType::Reference))
        .expect("seed");
    let tool = MemoryArchiveTool::new_pinned(
        Arc::clone(&mem) as Arc<dyn MemoryProvider>,
        MemoryScope::Auto,
    );
    let props = tool.input_schema()["properties"]
        .as_object()
        .expect("props")
        .clone();
    assert!(!props.contains_key("scope"));
    let out = tool
        .execute(ToolCtx::new("c3"), json!({"key": "auto-item"}))
        .await
        .expect("pinned archive");
    assert_eq!(out, json!({"archived": "auto-item", "scope": "auto"}));
}
