//! Runs bounded memory extraction in an ephemeral session.
//!
//! The model and memory provider are shared with the main runner, while the
//! conversation log remains isolated.

use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_context::{SessionId, SessionLogEntry};
use std::path::Path;
use std::sync::Arc;

use super::extractor::extraction_window::ExactExtractionWindow;
use super::memory::MutationLog;
use super::prompt::extract::build_extraction_prompt;
use super::runner_config::RunnerConfig;
use super::{RunError, RunOutcome, RunResult, Runner, ToolRegistry};
#[cfg(test)]
use std::env::temp_dir;
#[cfg(test)]
use std::fs::create_dir_all;
#[cfg(test)]
use std::fs::remove_dir_all;
#[cfg(test)]
use std::process::id;

/// Build a forked extraction runner over a caller-provided ephemeral store.
/// The store is ephemeral (an in-memory backend the caller constructs) so the
/// forked transcript stays out of the durable main log. The provider and
/// memory are shared (Arc clone) so prompt caching and the in-process write
/// lock carry over. The save_memory tool is registered with the given
/// recorder so the caller can drain the exact operations the fork landed
/// this pass, separately from the recorder the main runner's tool carries.
pub(crate) fn build_forked_extract_runner(
    store: Arc<dyn SessionLog>,
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: &Path,
    config: RunnerConfig,
    recorder: Arc<MutationLog>,
    evidence: Arc<[SessionLogEntry]>,
) -> Runner {
    let tools = ToolRegistry::new();
    Runner::new(store, provider, tools, config)
        .install_extraction_memory(memory, recorder, evidence)
        .with_cwd(cwd.to_path_buf())
}

/// Drive a forked extraction run on a fresh ephemeral session. The window's
/// events are the whole conversation the forked agent sees; no older history
/// is replayed, so every write cites evidence this pass can actually reach.
pub(crate) async fn run_forked_extract(
    store: Arc<dyn SessionLog>,
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: &Path,
    config: RunnerConfig,
    window: &ExactExtractionWindow<'_>,
    recorder: Arc<MutationLog>,
) -> Result<RunResult, RunError> {
    // Inject the existing-memory manifest so the forked agent dedups by
    // reusing a key instead of re-saving the same fact each turn (a
    // formatMemoryManifest pre-inject). Built before moving memory into
    // the runner.
    let prompt = build_extraction_prompt(&memory.list_memories());
    // The evidence window the save_memory tool validates quotes against:
    // the exact events the forked agent sees, owned so the Arc<dyn Tool>
    // outlives the borrowed ExactExtractionWindow.
    let evidence = Arc::from(window.events().to_vec());
    let runner =
        build_forked_extract_runner(store, provider, memory, cwd, config, recorder, evidence);
    let session = SessionId::new();
    let result = runner.run_forked(session, window.events(), prompt).await;
    // A fork that hits the turn cap did not finish extracting — treat as a
    // failure so the extractor's cursor stays and the range is reconsidered
    // next pass. The main loop's max_turns is a graceful RunOutcome; a
    // fork's is a failure (it did not finish the extraction pass).
    match result {
        Ok(RunResult {
            outcome: RunOutcome::MaxTurnsReached { turns },
            ..
        }) => Err(RunError::MaxTurnsExceeded { turns }),
        other => other,
    }
}

#[cfg(test)]
mod tests;
