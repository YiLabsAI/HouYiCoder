//! Child-agent spawn: create an independent child Runner from a parent's
//! components, record the durable spawn boundary, and return a handle.
//!
//! The spawn function is a standalone entry, not a method on Runner, so
//! the agent module file stays under the size gate. The caller extracts
//! what spawn needs into a SpawnRequest and passes it here; the child
//! Runner is built with a shared SessionStore (same backend, routes by
//! session id), the parent provider (shared), a narrowed ToolRegistry,
//! and a cloned RunnerConfig.

use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_async::CancellationToken;
use houyicoder_context::{SessionId, TurnEventKind};
use std::sync::Arc;

use crate::agent::append::new_event;
use crate::agent::runner_config::RunnerConfig;
use crate::agent::{Runner, ToolRegistry};

/// Cap on spawn nesting. A top-level agent is depth 0; each spawn adds 1.
/// v0 limit per the multi-agent config (max_depth = 4): levels 0-3 spawn
/// freely, a spawn at depth 4 (the 5th level) is rejected. Hardcoded here
/// until the config wire reaches this module.
const MAX_SPAWN_DEPTH: u32 = 4;

/// What the caller extracts from a parent Runner to spawn a child.
pub struct SpawnRequest {
    pub parent_sid: SessionId,
    pub parent_store: Arc<dyn SessionLog>,
    pub provider: Arc<dyn ModelProvider>,
    pub tools: ToolRegistry,
    pub config: RunnerConfig,
    pub subagent_type: String,
    pub prompt: String,
    pub prompt_summary: String,
    /// Spawn depth of the parent (0 = top-level). The runtime derives this
    /// from the session's spawn ancestry before calling spawn; the child is
    /// depth + 1. Used by the recursion guard to cap nesting.
    pub depth: u32,
}

/// A handle to a spawned child. Carries the child session id + a cancel
/// token the caller uses to abort the child run.
pub struct ChildHandle {
    pub session: SessionId,
    pub runner: Arc<Runner>,
    pub cancel: CancellationToken,
}

/// Why a spawn was rejected. Budget and capability failures are policy,
/// not panics; the caller surfaces these to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    BudgetExceeded,
    CapabilityDenied,
    SpawnRecursive,
    WorktreeFenceNarrowFail,
}

/// Spawn a child agent: create a fresh session, record the durable
/// SubagentSpawn boundary in the parent log, build the child Runner, and
/// return a handle. The child shares the parent SessionStore (the backend
/// routes by session id, so child events land in the child sidechain log,
/// not the parent log).
pub async fn spawn_child(req: SpawnRequest) -> Result<ChildHandle, SpawnError> {
    // Recursion guard: cap spawn nesting before any side effect. A reject
    // here must not write a boundary or build a child. Fork-recursion (a
    // fork-cache child spawning its own fork) is not reachable in v0, which
    // uses fresh-context children; the depth cap is the only active guard.
    if req.depth >= MAX_SPAWN_DEPTH {
        return Err(SpawnError::SpawnRecursive);
    }

    let child_sid = SessionId::new();
    let cancel = CancellationToken::new();

    // Record the spawn boundary in the parent log before building the
    // child. The durable boundary is part of the spawn fence: a failed
    // write fail-closes the spawn (replay must not lose the boundary, so
    // refuse to proceed without it). Worktree-fence setup failures route
    // here too; the boundary write is the fence record.
    let spawn_event = new_event(
        req.parent_sid,
        TurnEventKind::SubagentSpawn {
            child_session_id: child_sid.to_string(),
            subagent_type: req.subagent_type.clone(),
            prompt_summary: req.prompt_summary.clone(),
            isolation: "none".to_string(),
            policy: "delegate".to_string(),
        },
    );
    req.parent_store
        .append(spawn_event)
        .await
        .map_err(|_| SpawnError::WorktreeFenceNarrowFail)?;

    // The child shares the parent store. The backend routes by session
    // id: the child appends with child_sid, so its events land in the
    // child's own log, not the parent's. This is the sidechain model.
    let runner = Arc::new(Runner::with_shared_store(
        req.parent_store,
        req.provider,
        req.tools,
        req.config,
    ));

    Ok(ChildHandle {
        session: child_sid,
        runner,
        cancel,
    })
}

#[cfg(test)]
#[path = "spawn_tests.rs"]
mod tests;
