//! Constructs the memory runtime and its persistent provider.

use std::env::var;
use std::path::{Path, PathBuf};

use super::*;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::path::home_directory;
use houyicoder_api::provider::ModelProvider;
use houyicoder_core::agent::{MemoryGates, MemoryRuntime};
use houyicoder_memory::MarkdownMemoryProvider;
use houyicoder_memory::SemanticReranker;

/// The config root the provider's three scopes hang under. An explicit home
/// override wins, then a non-empty config-home environment value, then the
/// user's home directory, and finally the workspace itself. The environment
/// arm matters because a test that isolates only the config home still must
/// not create directories under the real user tree.
fn memory_config_root(
    ws: &Path,
    home_override: Option<&Path>,
    env_root: Option<PathBuf>,
) -> PathBuf {
    home_override
        .map(|home| home.join(".houyicoder"))
        .or_else(|| env_root.filter(|root| !root.as_os_str().is_empty()))
        .or_else(|| home_directory().map(|home| home.join(".houyicoder")))
        .unwrap_or_else(|| ws.join(".houyicoder"))
}

/// Build a three-scope provider and repair its derived index. The home
/// override exists so tests inject an isolated root instead of reading the
/// real user home; production passes None and the config-home environment
/// value or the environment home applies.
pub(super) fn memory_provider_for(
    ws: &Path,
    home_override: Option<&Path>,
) -> MarkdownMemoryProvider {
    let (toggles, _) = houyicoder_config::load_toggles();
    let config_root = memory_config_root(
        ws,
        home_override,
        var(houyicoder_config::ENV_HOUYICODER_CONFIG_HOME)
            .ok()
            .map(PathBuf::from),
    );
    let slug = worktree::git_canonical_slug(ws);
    let user_root = config_root.join("memory");
    // The main worktree's directory, not the active one: a linked worktree
    // otherwise reads an empty project scope while its siblings hold the
    // records.
    let project_root = worktree::git_common_root(ws)
        .unwrap_or_else(|| ws.to_path_buf())
        .join(".houyicoder")
        .join("memory");
    let auto_root = config_root.join("projects").join(&slug).join("memory");
    let provider = MarkdownMemoryProvider::new_multi(vec![user_root, project_root, auto_root])
        .with_auto_expire(toggles.auto_expire);
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

pub(super) fn build_workspace_memory(
    workspace: Option<(PathBuf, Option<PathBuf>)>,
    store: Arc<dyn SessionLog>,
    session: SessionId,
    provider_for_extractor: Arc<dyn ModelProvider>,
    model_for_extractor: String,
    session_log_root: Option<PathBuf>,
) -> (Option<(PathBuf, MemoryRuntime)>, Vec<ConfigWarning>) {
    match workspace {
        Some((ws, home)) => {
            let memory_provider: Arc<dyn MemoryProvider> =
                Arc::new(memory_provider_for(&ws, home.as_deref()));
            let (runtime, warnings) = build_memory_runtime(
                store,
                session,
                memory_provider,
                provider_for_extractor,
                ws.clone(),
                model_for_extractor,
                session_log_root,
            );
            (Some((ws, runtime)), warnings)
        }
        None => (None, Vec::new()),
    }
}

pub(super) fn attach_workspace_memory(
    runner: Runner,
    workspace_memory: Option<(PathBuf, MemoryRuntime)>,
    undo_handles: Option<(Arc<Mutex<UndoStack>>, Arc<SnapshotStore>)>,
    worktree_controller: Option<&Arc<WorktreeController>>,
) -> Runner {
    match workspace_memory {
        Some((ws, runtime)) => {
            let mut r = runner.with_cwd(ws).install_memory(runtime);
            if let Some((stack, store)) = undo_handles {
                r.set_undo(stack, store);
            }
            if let Some(controller) = worktree_controller {
                controller.set_cwd_handle(r.cwd_handle());
            }
            r
        }
        None => runner,
    }
}

#[cfg(test)]
mod tests;
