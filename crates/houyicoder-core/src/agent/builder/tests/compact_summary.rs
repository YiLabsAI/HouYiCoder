//! Tests for this module.

use super::super::*;
use houyicoder_context::{
    CheckpointId, CheckpointManifest, Disposition, EventId, SessionEvent, SessionId,
    SessionLogEntry, TurnGroup,
};
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;

fn runner_with_store() -> (Runner, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let runner = Runner::new(
        store,
        Arc::new(crate::provider::test_support::FakeProvider::new(vec![])),
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig::default(),
    );
    (runner, session)
}

#[tokio::test]
async fn test_compact_summary_no_checkpoint() {
    let (runner, session) = runner_with_store();
    assert_eq!(runner.compact_summary(session).await, None);
}

#[tokio::test]
async fn test_compact_summary_formats_manifest() {
    let (runner, session) = runner_with_store();
    let event = SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::AssistantMessage {
            text: "folded".into(),
            thinking: None,
        },
    };
    runner.store().append(event.clone()).await.unwrap();
    let manifest = CheckpointManifest {
        id: CheckpointId::new(),
        session,
        last_event: event.id,
        summary: Some("folded earlier turns".into()),
        plan: vec![TurnGroup {
            turn_id: event.id,
            disposition: Disposition::Summarized,
            event_ids: vec![event.id],
        }],
        ts: 0,
    };
    runner
        .store()
        .backend()
        .write_checkpoint(manifest)
        .await
        .unwrap();
    let summary = runner.compact_summary(session).await;
    assert!(summary.is_some(), "compact summary should be Some");
    let s = summary.unwrap();
    assert!(s.contains("1 compacts"), "{s}");
    assert!(s.contains("1 turns folded"), "{s}");
    assert!(s.contains("folded earlier turns"), "{s}");
}

#[tokio::test]
async fn test_compact_tokens_no_checkpoint() {
    let (runner, session) = runner_with_store();
    assert_eq!(runner.compact_summary_tokens(session).await, 0);
}

#[tokio::test]
async fn test_compact_summary_tokens_none() {
    let (runner, session) = runner_with_store();
    let event = SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::AssistantMessage {
            text: "folded".into(),
            thinking: None,
        },
    };
    runner.store().append(event.clone()).await.unwrap();
    let manifest = CheckpointManifest {
        id: CheckpointId::new(),
        session,
        last_event: event.id,
        summary: None,
        plan: vec![TurnGroup {
            turn_id: event.id,
            disposition: Disposition::Summarized,
            event_ids: vec![event.id],
        }],
        ts: 0,
    };
    runner
        .store()
        .backend()
        .write_checkpoint(manifest)
        .await
        .unwrap();
    assert_eq!(
        runner.compact_summary_tokens(session).await,
        0,
        "None summary -> 0 tokens"
    );
}

#[tokio::test]
async fn test_compact_summary_tokens_counts() {
    let (runner, session) = runner_with_store();
    let event = SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::AssistantMessage {
            text: "folded".into(),
            thinking: None,
        },
    };
    runner.store().append(event.clone()).await.unwrap();
    let manifest = CheckpointManifest {
        id: CheckpointId::new(),
        session,
        last_event: event.id,
        summary: Some("this is a summary of folded turns".into()),
        plan: vec![TurnGroup {
            turn_id: event.id,
            disposition: Disposition::Summarized,
            event_ids: vec![event.id],
        }],
        ts: 0,
    };
    runner
        .store()
        .backend()
        .write_checkpoint(manifest)
        .await
        .unwrap();
    let tokens = runner.compact_summary_tokens(session).await;
    assert!(
        tokens > 0,
        "summary text produces non-zero tokens: {tokens}"
    );
}

#[test]
fn test_memory_index_without_provider() {
    let (runner, _) = runner_with_store();
    assert_eq!(runner.format_memory_index(), None);
}

#[test]
fn test_memory_index_formats_entries() {
    use crate::agent::MemoryRuntime;
    use houyicoder_api::memory::MemoryProvider;
    use houyicoder_context::{MemoryEntry, MemorySource};
    use houyicoder_memory::MarkdownMemoryProvider;
    let root = std::env::temp_dir().join(format!("mem-index-{}-{}", std::process::id(), line!()));
    drop(std::fs::remove_dir_all(&root));
    std::fs::create_dir_all(&root).expect("mkdir");
    let memory: Arc<dyn MemoryProvider> = Arc::new(MarkdownMemoryProvider::new(root.clone()));
    memory
        .add(MemoryEntry::new(
            "proj-pref",
            "prefer let chains",
            MemorySource::Project,
        ))
        .unwrap();
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let mut runtime = MemoryRuntime::new(store.clone());
    runtime.install_provider(memory.clone());
    let runner = Runner::new(
        store,
        Arc::new(crate::provider::test_support::FakeProvider::new(vec![])),
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig::default(),
    )
    .install_memory(runtime);
    let idx = runner.format_memory_index();
    assert!(idx.is_some(), "configured provider produces an index");
    let s = idx.unwrap();
    assert!(s.contains("proj-pref"), "key in index: {s}");
    assert!(s.contains("project"), "source label: {s}");
    assert!(s.contains("prefer let chains"), "description: {s}");

    memory
        .add(MemoryEntry::new(
            "late-entry",
            "written after the first request",
            MemorySource::Project,
        ))
        .unwrap();
    let frozen = runner.format_memory_index().unwrap();
    assert!(
        !frozen.contains("late-entry"),
        "ordinary turns keep the prefix"
    );
    runner.memory.invalidate_index_snapshot();
    let refreshed = runner.format_memory_index().unwrap();
    assert!(
        refreshed.contains("late-entry"),
        "a cache-break refreshes the index"
    );
    drop(std::fs::remove_dir_all(&root));
}
