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
use crate::agent::worktree_controller::{ChildWorktree, WorktreeController};
use crate::agent::{Runner, ToolRegistry};

use super::child_prompt::resolve_child_effort;
use super::registry::IsolationMode;

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
    /// The isolation mode the resolved agent definition asked for. Worktree
    /// creates a per-child worktree + narrows the fence; None runs the child
    /// in the parent's tree.
    pub isolation: IsolationMode,
    /// The worktree controller, required when isolation is Worktree. None
    /// with Worktree is a wiring error the runtime must not produce; spawn
    /// fail-closes to WorktreeFenceNarrowFail rather than degrading to no
    /// isolation.
    pub worktree_controller: Option<Arc<WorktreeController>>,
    /// True for an async (unlinked) child that returns immediately and
    /// completes later; false for a sync child that blocks the parent turn.
    pub run_in_background: bool,
    /// The parent's cancel token, shared by a sync child so a parent abort
    /// cancels the child too. Ignored for an async child, which gets a fresh
    /// unlinked token so the parent's ESC does not propagate.
    pub parent_cancel: Option<CancellationToken>,
}

/// A handle to a spawned child. Carries the child session id + a cancel
/// token the caller uses to abort the child run. When the child was spawned
/// with worktree isolation, worktree holds the per-child fence guard; the
/// caller drops it on completion to restore the fence.
pub struct ChildHandle {
    pub session: SessionId,
    pub runner: Arc<Runner>,
    pub cancel: CancellationToken,
    pub worktree: Option<ChildWorktree>,
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

/// Spawn a child agent: create a fresh session, build the child Runner
/// sharing the parent SessionStore, record the durable SubagentSpawn
/// boundary in the parent log, and return a handle. The child shares the
/// parent SessionStore (the backend routes by session id, so child events
/// land in the child sidechain log, not the parent log).
pub async fn spawn_child(req: SpawnRequest) -> Result<ChildHandle, SpawnError> {
    // Recursion guard: cap spawn nesting before any side effect. A reject
    // here must not write a boundary or build a child. Fork-recursion (a
    // fork-cache child spawning its own fork) is not reachable in v0, which
    // uses fresh-context children; the depth cap is the only active guard.
    if req.depth >= MAX_SPAWN_DEPTH {
        return Err(SpawnError::SpawnRecursive);
    }

    let child_sid = SessionId::new();
    // A sync child shares the parent's cancel token (a linked clone), so a
    // parent abort cancels the child; an async child gets a fresh unlinked
    // token, so the parent's ESC does not propagate -- the caller cancels an
    // async child through its own handle. A sync spawn with no parent token
    // degrades to an unlinked child token (still cancellable via the handle,
    // just not parent-linked).
    let cancel = if req.run_in_background {
        CancellationToken::new()
    } else {
        req.parent_cancel.clone().unwrap_or_default()
    };

    // Per-child worktree + fence. Fail-closed: a missing controller or a
    // fence failure rejects the spawn rather than degrading to no isolation,
    // and no child session is built or boundary written. The guard travels
    // with the child handle; the caller drops it on completion to restore
    // the fence. The sync git + sandbox calls block the async caller for the
    // one-shot worktree-setup cost (a git add + fence narrow).
    let (worktree, isolation_str) = match req.isolation {
        IsolationMode::None => (None, "none"),
        IsolationMode::Worktree => {
            let controller = req
                .worktree_controller
                .as_ref()
                .ok_or(SpawnError::WorktreeFenceNarrowFail)?;
            let slug = format!("agent-{}", &child_sid.to_string()[..8]);
            let cw = controller
                .enter_for_child(slug)
                .map_err(|_| SpawnError::WorktreeFenceNarrowFail)?;
            (Some(cw), "worktree")
        }
    };

    // Build the child Runner before recording the boundary, so a build
    // failure leaves no dangling spawn in the parent log (resume would
    // reconcile a child that never existed). The boundary records the child
    // id minted above; a failed boundary write drops the unstarted runner
    // shell -- no child events were appended, no phantom remains. This
    // build-before-record order matches the design flow (isolation + child
    // runner, then boundary) and keeps the future worktree fence slot
    // before the boundary write, where a fence failure also leaves none.
    let store_for_boundary = req.parent_store.clone();
    let parent_sid = req.parent_sid;
    let subagent_type = req.subagent_type.clone();
    let prompt_summary = req.prompt_summary.clone();

    let runner = Arc::new(
        Runner::with_shared_store(req.parent_store, req.provider, req.tools, req.config)
            .with_agent_identity(houyicoder_api::spawn::AgentIdentity {
                subagent_type: Some(req.subagent_type.clone()),
                // The child dispatches carry the child's own identity, so a
                // nested agent call reports depth + 1 to the recursion guard.
                depth: req.depth + 1,
                parent_session_id: Some(req.parent_sid.to_string()),
            }),
    );
    // Children run at the lowest effort tier: a fan-out of sub-agents must
    // not multiply reasoning-token spend. Sticky on the runner so every
    // child request carries it; the active pick outranks any catalog level
    // the child model might default to.
    runner.set_effort(resolve_child_effort());

    let spawn_event = new_event(
        parent_sid,
        TurnEventKind::SubagentSpawn {
            child_session_id: child_sid.to_string(),
            subagent_type,
            prompt_summary,
            isolation: isolation_str.to_string(),
            policy: "delegate".to_string(),
        },
    );
    store_for_boundary
        .append(spawn_event)
        .await
        .map_err(|_| SpawnError::WorktreeFenceNarrowFail)?;

    Ok(ChildHandle {
        session: child_sid,
        runner,
        cancel,
        worktree,
    })
}

/// Append the SubagentReturn boundary to the parent log: the durable marker
/// that pairs with SubagentSpawn so replay reconstructs the delegation
/// (child reached a terminal state, with its summary + usage). The runtime
/// writes this after driving a sync child to terminal.
pub async fn record_subagent_return(
    store: &dyn SessionLog,
    parent_sid: SessionId,
    child_session_id: &str,
    status: &str,
    summary: &str,
    result_ref: &str,
    usage: &houyicoder_protocol::llm::Usage,
) -> Result<(), SpawnError> {
    let event = new_event(
        parent_sid,
        TurnEventKind::SubagentReturn {
            child_session_id: child_session_id.to_string(),
            status: status.to_string(),
            summary: summary.to_string(),
            result_ref: result_ref.to_string(),
            input_tokens: usage.input_tokens as u64,
            output_tokens: usage.output_tokens as u64,
            cache_read_input_tokens: usage.cache_read_input_tokens as u64,
            cache_write_input_tokens: usage.cache_write_input_tokens as u64,
            reasoning_tokens: usage.reasoning_tokens as u64,
        },
    );
    store
        .append(event)
        .await
        .map(|_| ())
        .map_err(|_| SpawnError::WorktreeFenceNarrowFail)
}

#[cfg(test)]
#[path = "spawn_tests.rs"]
mod tests;
