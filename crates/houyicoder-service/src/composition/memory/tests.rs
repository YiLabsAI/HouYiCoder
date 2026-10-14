//! Tests for this module.

use super::{build_memory_runtime, heal_memory_index, memory_provider_for};
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_memory::{InMemoryBackend, MarkdownMemoryProvider};
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

fn temp_root() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("memory_heal_{seq}_{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp root");
    dir
}

#[test]
fn test_heal_rebuilds_stale_root() {
    let root = temp_root();
    std::fs::write(
        root.join("topic.md"),
        "---\nname: t\ndescription: d\n---\nbody\n",
    )
    .expect("write topic");
    let provider = houyicoder_memory::MarkdownMemoryProvider::new(root.clone());
    assert!(
        !root.join("MEMORY.md").exists(),
        "precondition: no index yet"
    );
    heal_memory_index(&provider);
    assert!(
        root.join("MEMORY.md").exists(),
        "index rebuilt by self-heal"
    );
    drop(std::fs::remove_dir_all(&root));
}

#[test]
fn test_heal_noop_empty_root() {
    let root = temp_root();
    let provider = houyicoder_memory::MarkdownMemoryProvider::new(root.clone());
    heal_memory_index(&provider);
    assert!(
        !root.join("MEMORY.md").exists(),
        "no index written for an empty root"
    );
    drop(std::fs::remove_dir_all(&root));
}

#[tokio::test]
async fn test_runtime_seeds_extractor_cursor() {
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let last = EventId::new();
    store
        .append(SessionLogEntry {
            id: last,
            session,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: "restored".into(),
            },
        })
        .await
        .expect("append");
    let root = temp_root();
    let memory: Arc<dyn MemoryProvider> = Arc::new(MarkdownMemoryProvider::new(root.clone()));
    let model: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let (runtime, _warnings) = build_memory_runtime(
        Arc::clone(&store),
        session,
        memory,
        model,
        std::env::temp_dir(),
        "test-model".into(),
        None,
    );
    assert_eq!(
        runtime.extractor_cursor(),
        Some(last),
        "the restored tail seeds the extraction cursor"
    );
    // A session the store cannot answer for keeps the count-all cursor.
    let root2 = temp_root();
    let fresh = SessionId::new();
    let (runtime, _warnings) = build_memory_runtime(
        store,
        fresh,
        Arc::new(MarkdownMemoryProvider::new(root2.clone())),
        Arc::new(FakeProvider::text("ok")),
        std::env::temp_dir(),
        "test-model".into(),
        None,
    );
    assert_eq!(
        runtime.extractor_cursor(),
        None,
        "an unknown session seeds nothing"
    );
    drop(std::fs::remove_dir_all(&root));
    drop(std::fs::remove_dir_all(&root2));
}

#[tokio::test]
async fn test_extractor_reads_durable_frontier() {
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let prompt = EventId::new();
    store
        .append(SessionLogEntry {
            id: prompt,
            session,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput { text: "hi".into() },
        })
        .await
        .expect("append");
    let root = temp_root();
    let memory: Arc<dyn MemoryProvider> = Arc::new(MarkdownMemoryProvider::new(root.clone()));
    let model: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let (runtime, _warnings) = build_memory_runtime(
        Arc::clone(&store),
        session,
        memory,
        model,
        std::env::temp_dir(),
        "test-model".into(),
        None,
    );
    assert_eq!(
        runtime.extractor_frontier(session),
        Some(prompt),
        "the pass classifies against the durable log, not the forked run's log"
    );
    drop(std::fs::remove_dir_all(&root));
}

#[cfg(unix)]
#[test]
fn test_heal_logs_write_failure() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_root();
    std::fs::write(
        root.join("topic.md"),
        "---\nname: t\ndescription: d\n---\nbody\n",
    )
    .expect("write topic");
    // Make the root read-only so the index pointer write fails.
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).expect("set read-only");
    let provider = houyicoder_memory::MarkdownMemoryProvider::new(root.clone());
    // Must not panic; the failure is logged, not propagated.
    heal_memory_index(&provider);
    // Restore so cleanup can remove the dir.
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).ok();
    drop(std::fs::remove_dir_all(&root));
}

/// An injected home override routes every provider root under the
/// override, so a test never touches the real user memory directory:
/// a save lands under the override, not under the environment home.
#[test]
fn test_roots_honor_home_override() {
    use super::super::worktree;
    use houyicoder_api::memory::MemoryProvider;
    use houyicoder_context::{MemoryEntry, MemorySource};
    let isolated = temp_root();
    let ws = temp_root();
    let provider = memory_provider_for(&ws, Some(&isolated));
    provider
        .add(MemoryEntry::new(
            "isolated-key",
            "written under the override",
            MemorySource::Project,
        ))
        .expect("save lands");
    let override_user = isolated.join(".houyicoder").join("memory");
    let override_auto_projects = isolated.join(".houyicoder").join("projects");
    assert!(
        override_user.join("isolated-key.md").exists()
            || override_auto_projects
                .join(worktree::git_canonical_slug(&ws))
                .join("memory")
                .join("isolated-key.md")
                .exists(),
        "the write must land inside the override root"
    );
    drop(fs::remove_dir_all(&isolated));
    drop(fs::remove_dir_all(&ws));
}
