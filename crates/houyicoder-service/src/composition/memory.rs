//! Memory wiring for the composition root.
//!
//! Split out of the composition module on size grounds: that file is the sole
//! composition root and an acknowledged churn magnet, so it stays under the
//! per-file gate by moving whole concerns out rather than by trimming prose. The
//! functions here are the memory half of the wiring and nothing else consumes
//! them, so they form a seam that can move without touching a call site outside
//! the composition root.

use super::*;

/// Build the markdown memory provider wired to three scopes (user / project /
/// auto) — recall merges, writes land in the auto scope. The derived index is
/// self-healed before return so a crash between runs cannot leave a drifted
/// index at the first turn.
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

/// Build the memory extractor the runner fires at query-loop end. Shares
/// the runner's provider (prompt cache) + memory (write lock); the forked
/// transcript lands in a fresh ephemeral store so the main log is never
/// touched. max_turns five bounds the fork against rabbit-holing. Extracted
/// so build_runner stays under the function-line cap.
fn build_memory_extractor(
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: std::path::PathBuf,
    model: String,
) -> Arc<MemoryExtractor> {
    let ephemeral: Arc<dyn SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let max_output_tokens = houyicoder_core::agent::model_window::resolve_max_output_tokens(&model);
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

/// Wire background memory (extractor + dream) at query-loop end, fire-and-forget. Extracted to keep build_runner under the line cap.
/// Returns the runner plus any warnings the toggles load produced so the
/// composition root can surface them (a bad toggle value must not silently
/// become a no-op).
pub(super) fn wire_background_memory(
    runner: Runner,
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: std::path::PathBuf,
    model: String,
) -> (Runner, Vec<houyicoder_config::ConfigWarning>) {
    let (toggles, settings_warnings) = houyicoder_config::load_toggles();
    let auto_memory = Arc::new(std::sync::atomic::AtomicBool::new(toggles.auto_memory));
    let auto_dream = Arc::new(std::sync::atomic::AtomicBool::new(toggles.auto_dream));
    let runner = runner
        .with_toggles(auto_memory, auto_dream)
        .with_extractor(build_memory_extractor(
            Arc::clone(&provider),
            Arc::clone(&memory),
            cwd.clone(),
            model.clone(),
        ))
        .with_dream(build_dream_runner(
            provider,
            memory,
            cwd,
            model,
            super::session_log_root(),
        ));
    (runner, settings_warnings)
}

/// Build the consolidation dream firing at query-loop end. Shares provider + memory; ephemeral store; max_turns 25.
fn build_dream_runner(
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: std::path::PathBuf,
    model: String,
    session_log_root: std::path::PathBuf,
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
    Arc::new(
        DreamRunner::new(ephemeral, provider, memory, cwd, config)
            .with_session_log_root(session_log_root),
    )
}

/// Self-heal the memory index at session start. Best-effort: a failure logs
/// and the store still serves from the topic files (scan works without an
/// index). Extracted so the wiring is unit-testable without constructing a
/// full Runner (build_runner wires real providers and network).
pub(super) fn heal_memory_index(provider: &houyicoder_memory::MarkdownMemoryProvider) {
    if let Err(e) = houyicoder_api::memory::MemoryProvider::rebuild_if_stale(provider) {
        tracing::warn!("memory self-heal failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::heal_memory_index;

    fn temp_root() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("memory_heal_{seq}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    /// A root with a topic file and no index is stale; heal must rebuild the
    /// derived MEMORY.md so recall sees the topic on the first turn.
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

    /// An empty root (no topics) is not stale; heal is a no-op and writes no
    /// index. Guards against a regression that eagerly rebuilds nothing into
    /// a spurious empty index.
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

    /// A rebuild failure (the root is read-only so the index write fails)
    /// must not panic — the error path logs and returns so the store still
    /// serves from the topic files. Covers the best-effort error branch.
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
