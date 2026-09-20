//! Constructs the memory runtime and its persistent provider.

use super::*;
use houyicoder_core::agent::{MemoryGates, MemoryRuntime};

/// Build a three-scope provider and repair its derived index.
pub(super) fn memory_provider_for(
    ws: &std::path::Path,
) -> houyicoder_memory::MarkdownMemoryProvider {
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| ws.to_path_buf());
    let slug = worktree::git_canonical_slug(ws);
    let user_root = home.join(".houyicoder").join("memory");
    let project_root = ws.join(".houyicoder").join("memory");
    let auto_root = home
        .join(".houyicoder")
        .join("projects")
        .join(&slug)
        .join("memory");
    let provider = houyicoder_memory::MarkdownMemoryProvider::new_multi(vec![
        user_root,
        project_root,
        auto_root,
    ]);
    heal_memory_index(&provider);
    provider
}

/// Build an isolated, bounded memory extractor.
fn build_memory_extractor(
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: std::path::PathBuf,
    model: String,
) -> Arc<MemoryExtractor> {
    let ephemeral: Arc<dyn SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let max_output_tokens = model_window::resolve_max_output_tokens(&model);
    let config = RunnerConfig {
        model,
        instructions: String::new(),
        max_turns: 5,
        max_output_tokens,
        ..RunnerConfig::default()
    };
    Arc::new(MemoryExtractor::new(
        ephemeral, provider, memory, cwd, config,
    ))
}

/// Build a configured memory runtime and return settings warnings.
pub(super) fn build_memory_runtime(
    store: Arc<dyn SessionLog>,
    session: SessionId,
    provider: Arc<dyn MemoryProvider>,
    model_provider: Arc<dyn ModelProvider>,
    cwd: std::path::PathBuf,
    model: String,
    session_log_root: Option<std::path::PathBuf>,
) -> (MemoryRuntime, Vec<houyicoder_config::ConfigWarning>) {
    let (toggles, settings_warnings) = houyicoder_config::load_toggles();
    let gates = MemoryGates::new(toggles.auto_memory, toggles.auto_dream);
    let extractor = build_memory_extractor(
        Arc::clone(&model_provider),
        Arc::clone(&provider),
        cwd.clone(),
        model.clone(),
    );
    // Seed the cursor to the restored tail so the first pass counts only
    // messages that arrive after startup, not the history this process never watched.
    if let Some(id) = store.last_trajectory_id(session) {
        extractor.seed_cursor(id);
    }
    let dream = build_dream_runner(
        model_provider,
        Arc::clone(&provider),
        cwd,
        model,
        session_log_root,
    );
    let runtime =
        MemoryRuntime::from_parts(store, Some(provider), gates, Some(extractor), Some(dream));
    (runtime, settings_warnings)
}

/// Build an isolated consolidation worker with bounded turns.
fn build_dream_runner(
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: std::path::PathBuf,
    model: String,
    session_log_root: Option<std::path::PathBuf>,
) -> Arc<DreamRunner> {
    let ephemeral: Arc<dyn SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let max_output_tokens = model_window::resolve_max_output_tokens(&model);
    let config = RunnerConfig {
        model,
        instructions: String::new(),
        max_turns: DEFAULT_DREAM_MAX_TURNS,
        max_output_tokens,
        ..RunnerConfig::default()
    };
    let mut dream = DreamRunner::new(ephemeral, provider, memory, cwd, config);
    if let Some(root) = session_log_root {
        dream = dream.with_session_log_root(root);
    }
    Arc::new(dream)
}

/// Repair a stale derived index without preventing startup.
pub(super) fn heal_memory_index(provider: &houyicoder_memory::MarkdownMemoryProvider) {
    if let Err(e) = houyicoder_api::memory::MemoryProvider::rebuild_if_stale(provider) {
        tracing::warn!("memory self-heal failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::{build_memory_runtime, heal_memory_index};
    use houyicoder_api::memory::MemoryProvider;
    use houyicoder_api::provider::ModelProvider;
    use houyicoder_api::session::SessionLog;
    use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
    use houyicoder_memory::{InMemoryBackend, MarkdownMemoryProvider};
    use houyicoder_provider::FakeProvider;
    use houyicoder_session::SessionStore;
    use std::sync::Arc;

    fn temp_root() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("memory_heal_{seq}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp root");
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
        let store: Arc<dyn SessionLog> =
            Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
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
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555))
            .expect("set read-only");
        let provider = houyicoder_memory::MarkdownMemoryProvider::new(root.clone());
        // Must not panic; the failure is logged, not propagated.
        heal_memory_index(&provider);
        // Restore so cleanup can remove the dir.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).ok();
        drop(std::fs::remove_dir_all(&root));
    }
}
