//! The agent-spawn port. ToolCtx lives in the port layer and cannot
//! reference the engine's spawn request, which carries runner components
//! a tool cannot see, so the spawn contract stays neutral here: the
//! engine adapts these types to its full request and child handle.

use houyicoder_async::PFut;

/// The running agent's identity, threaded through ToolCtx so the agent
/// tool can enforce the recursion guard and fork-recursion check without
/// holding the runner.
#[derive(Debug, Clone)]
pub struct AgentIdentity {
    pub subagent_type: Option<String>,
    /// 0 for a top-level agent; the agent tool passes depth+1 into the
    /// spawn so the guard caps nesting without trusting the caller's depth
    /// beyond the runtime that derives it from the session chain.
    pub depth: u32,
    pub parent_session_id: Option<String>,
}

impl AgentIdentity {
    /// The identity of a top-level runner: depth 0, no subagent type.
    pub fn top_level() -> Self {
        Self {
            subagent_type: None,
            depth: 0,
            parent_session_id: None,
        }
    }
}

/// The agent-facing spawn request, handed to the spawn handle. The
/// engine's full spawn request adds parent components a tool cannot see
/// (store, provider, tool registry, config); the handle impl supplies
/// those from its own state. Non-exhaustive because the agent tool's
/// schema grows (name, cwd, model land in later sprints); construct via
/// new so a field added later does not break every call site.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SpawnArgs {
    pub subagent_type: String,
    pub prompt: String,
    /// Short summary for the durable spawn boundary in the parent log;
    /// replay reconstructs the spawn without the full prompt.
    pub prompt_summary: String,
    /// "none" for a fresh-context child, "worktree" for a per-child fence;
    /// the engine serializes its typed isolation to this string here.
    pub isolation: String,
    /// False: spawn blocks until the child reaches a terminal state (sync;
    /// the caller reads the child log by sid for status, summary, result
    /// ref, and usage). True: spawn returns once the child is started
    /// (async; completion arrives later as a pending notification on the
    /// next parent turn).
    pub run_in_background: bool,
}

impl SpawnArgs {
    /// Build from the three fields the model provides; isolation and
    /// run_in_background default to a sync, fresh-context spawn.
    pub fn new(
        subagent_type: impl Into<String>,
        prompt: impl Into<String>,
        prompt_summary: impl Into<String>,
    ) -> Self {
        Self {
            subagent_type: subagent_type.into(),
            prompt: prompt.into(),
            prompt_summary: prompt_summary.into(),
            isolation: "none".to_string(),
            run_in_background: false,
        }
    }
}

/// The child reference a spawn returns. The engine's full handle carries
/// the child Runner; the port exposes only the session id, which is all a
/// tool needs to reference the child.
#[derive(Debug, Clone)]
pub struct SpawnOutcome {
    pub child_session_id: String,
}

/// Why a spawn was rejected, mirroring the engine's SpawnError but neutral
/// so the port does not depend on the engine crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnFailure {
    BudgetExceeded,
    CapabilityDenied,
    Recursive,
    FenceFail,
}

/// The spawn port a tool calls through its ToolCtx. The engine (the runner
/// or a multi-agent runtime) implements it, adapting SpawnArgs to its full
/// spawn request and the child handle back to SpawnOutcome.
pub trait SpawnHandle: Send + Sync {
    /// Spawn a child agent; returns the child session id on success or a
    /// typed rejection the tool surfaces to the model.
    ///
    /// The per-call ToolCtx supplies the parent session id (the child log's
    /// parent), the parent's cancel token (a sync child links it so a parent
    /// abort cancels the child), and the parent agent identity (the child's
    /// depth is parent + 1 for the recursion guard).
    ///
    /// Timing is part of the contract: with run_in_background false, spawn
    /// awaits the child's terminal state before returning -- the caller then
    /// reads the child log by session id for status, summary, result ref,
    /// and usage. With it true, spawn returns once the child is started;
    /// completion reaches the parent later as a pending notification on the
    /// next parent turn. Either way the outcome carries only the child
    /// session id; the engine keeps the runner and cancel token.
    fn spawn(
        &self,
        ctx: &crate::tool::ToolCtx,
        args: SpawnArgs,
    ) -> PFut<'_, Result<SpawnOutcome, SpawnFailure>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoSpawn;

    impl SpawnHandle for NoSpawn {
        fn spawn(
            &self,
            _ctx: &crate::tool::ToolCtx,
            _args: SpawnArgs,
        ) -> PFut<'_, Result<SpawnOutcome, SpawnFailure>> {
            Box::pin(async { Err(SpawnFailure::Recursive) })
        }
    }

    #[tokio::test]
    async fn test_spawn_handle_callable() {
        let handle: Box<dyn SpawnHandle> = Box::new(NoSpawn);
        let args = SpawnArgs::new("explore", "find auth", "find auth");
        assert_eq!(args.isolation, "none");
        assert!(!args.run_in_background);
        let ctx = crate::tool::ToolCtx::new("c1");
        let outcome = handle.spawn(&ctx, args).await;
        assert!(matches!(outcome, Err(SpawnFailure::Recursive)));
    }

    #[test]
    fn test_toolctx_carries_spawn_context() {
        use crate::tool::ToolCtx;
        let identity = AgentIdentity {
            subagent_type: Some("explore".to_string()),
            depth: 1,
            parent_session_id: Some("parent-sid".to_string()),
        };
        let handle: std::sync::Arc<dyn SpawnHandle> = std::sync::Arc::new(NoSpawn);
        let ctx = ToolCtx::new("call-1")
            .with_agent_identity(identity)
            .with_spawn_handle(handle);
        assert_eq!(ctx.agent_identity.as_ref().unwrap().depth, 1);
        assert_eq!(
            ctx.agent_identity
                .as_ref()
                .unwrap()
                .subagent_type
                .as_deref(),
            Some("explore")
        );
        assert!(ctx.spawn_handle.is_some());
        assert!(ToolCtx::new("call-2").agent_identity.is_none());
        assert!(ToolCtx::new("call-3").spawn_handle.is_none());
    }
}
