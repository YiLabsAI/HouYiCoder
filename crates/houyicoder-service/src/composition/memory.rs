//! Constructs the memory runtime and its persistent provider.

use std::path::{Path, PathBuf};

use super::*;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::ModelProvider;
use houyicoder_core::agent::{MemoryGates, MemoryRuntime};
use houyicoder_memory::MarkdownMemoryProvider;
use houyicoder_memory::SemanticReranker;
use std::env::var;

/// Build a three-scope provider and repair its derived index. The home
/// override exists so tests inject an isolated root instead of reading the
/// real user home; production passes None and the environment home applies.
pub(super) fn memory_provider_for(
    ws: &Path,
    home_override: Option<&Path>,
) -> MarkdownMemoryProvider {
    let home: PathBuf = home_override
        .map(Path::to_path_buf)
        .or_else(|| var("HOME").ok().map(PathBuf::from))
        .unwrap_or_else(|| ws.to_path_buf());
    let slug = worktree::git_canonical_slug(ws);
    let user_root = home.join(".houyicoder").join("memory");
    let project_root = ws.join(".houyicoder").join("memory");
    let auto_root = home
        .join(".houyicoder")
        .join("projects")
        .join(&slug)
        .join("memory");
    let provider = MarkdownMemoryProvider::new_multi(vec![user_root, project_root, auto_root]);
    heal_memory_index(&provider);
    provider
}

/// Build an isolated, bounded memory extractor. The durable store is the one
/// the pass classifies its changes against; the ephemeral store is where the
/// forked run writes, so a pass never appends to the user's history.
fn build_memory_extractor(
    session_log: Arc<dyn SessionLog>,
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: PathBuf,
    model: String,
) -> Arc<MemoryExtractor> {
    let fork: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let max_output_tokens = model_window::resolve_max_output_tokens(&model);
    let config = RunnerConfig {
        model,
        instructions: String::new(),
        max_turns: 5,
        max_output_tokens,
        ..RunnerConfig::default()
    };
    Arc::new(MemoryExtractor::new(
        ExtractionLogs {
            session: session_log,
            fork,
        },
        provider,
        memory,
        cwd,
        config,
    ))
}

/// Build a configured memory runtime and return settings warnings.
pub(super) fn build_memory_runtime(
    store: Arc<dyn SessionLog>,
    session: SessionId,
    provider: Arc<dyn MemoryProvider>,
    model_provider: Arc<dyn ModelProvider>,
    cwd: PathBuf,
    model: String,
    session_log_root: Option<PathBuf>,
) -> (MemoryRuntime, Vec<houyicoder_config::ConfigWarning>) {
    let (toggles, settings_warnings) = houyicoder_config::load_toggles();
    let gates = MemoryGates::new(toggles.auto_memory, toggles.auto_dream);
    let extractor = build_memory_extractor(
        Arc::clone(&store),
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
    // The semantic selection stage shares the main model; recall fires it
    // only on a weak lexical signal, off the first-token path.
    let reranker = Arc::new(SemanticReranker::new(
        Arc::clone(&model_provider),
        model.clone(),
    ));
    let dream = build_dream_runner(
        model_provider,
        Arc::clone(&provider),
        cwd,
        model,
        session_log_root,
    );
    let mut runtime =
        MemoryRuntime::from_parts(store, Some(provider), gates, Some(extractor), Some(dream));
    runtime.install_reranker(reranker);
    (runtime, settings_warnings)
}

/// Build an isolated consolidation worker with bounded turns.
fn build_dream_runner(
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: PathBuf,
    model: String,
    session_log_root: Option<PathBuf>,
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
pub(super) fn heal_memory_index(provider: &MarkdownMemoryProvider) {
    if let Err(e) = MemoryProvider::rebuild_if_stale(provider) {
        tracing::warn!("memory self-heal failed: {e}");
    }
}

#[cfg(test)]
mod tests;
