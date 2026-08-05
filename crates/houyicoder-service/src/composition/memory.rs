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
