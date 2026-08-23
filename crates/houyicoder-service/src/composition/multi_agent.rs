//! The multi-agent runtime: the SpawnHandle impl the agent tool calls.
//! Holds the parent's components (registry, store, provider, tools, config,
//! worktree controller, cwd) and on a sync spawn drives the child runner to
//! a terminal state, records the SubagentReturn boundary, and returns the
//! result the tool projects into its tool_result.

use std::path::PathBuf;
use std::sync::Arc;

use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_api::spawn::{SpawnArgs, SpawnFailure, SpawnHandle, SpawnOutcome};
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::PFut;
use houyicoder_context::SessionId;
use houyicoder_core::agent::multi_agent::child_prompt::{child_system_prompt, child_user_context};
use houyicoder_core::agent::multi_agent::registry::{
    AgentError, AgentRegistry, IsolationMode, PromptSource, ResolveCtx,
};
use houyicoder_core::agent::multi_agent::{
    SpawnError, SpawnRequest, record_subagent_return, spawn_child,
};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::worktree_controller::WorktreeController;
use houyicoder_core::agent::{RunOutcome, RunResult, ToolRegistry};
use houyicoder_protocol::llm::Usage;

pub struct MultiAgentRuntime {
    registry: Arc<dyn AgentRegistry>,
    store: Arc<dyn SessionLog>,
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
    config: RunnerConfig,
    worktree_controller: Option<Arc<WorktreeController>>,
    cwd: PathBuf,
}

impl MultiAgentRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<dyn AgentRegistry>,
        store: Arc<dyn SessionLog>,
        provider: Arc<dyn ModelProvider>,
        tools: ToolRegistry,
        config: RunnerConfig,
        worktree_controller: Option<Arc<WorktreeController>>,
        cwd: PathBuf,
    ) -> Self {
        Self {
            registry,
            store,
            provider,
            tools,
            config,
            worktree_controller,
            cwd,
        }
    }
}

/// The built-in agent registry: the five built-in agent definitions. Shared
/// between the agent tool (resolve) and the spawn runtime (materialize) so
/// the two see one source of truth.
pub(super) fn built_in_registry() -> Arc<dyn AgentRegistry> {
    Arc::new(
        houyicoder_core::agent::multi_agent::registry::BuiltInRegistry::from_agents(
            houyicoder_core::agent::multi_agent::registry::built_in_all(),
        ),
    )
}

/// Build the spawn port the composition root attaches to the runner. The
/// workspace (or the process cwd when none resolved) is the child's env-block
/// cwd. Splits the runtime construction out of the composition root so the
/// root file stays under the size gate.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_runtime(
    registry: Arc<dyn AgentRegistry>,
    store: Arc<dyn SessionLog>,
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
    config: RunnerConfig,
    worktree_controller: Option<Arc<WorktreeController>>,
    workspace: Option<PathBuf>,
) -> Arc<dyn SpawnHandle> {
    let cwd = workspace.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    Arc::new(MultiAgentRuntime::new(
        registry,
        store,
        provider,
        tools,
        config,
        worktree_controller,
        cwd,
    ))
}

impl SpawnHandle for MultiAgentRuntime {
    fn spawn(
        &self,
        ctx: &ToolCtx,
        args: SpawnArgs,
    ) -> PFut<'_, Result<SpawnOutcome, SpawnFailure>> {
        // Extract the per-call context into owned values before the async
        // block so the returned future borrows nothing (its lifetime is that
        // of &self, but the captures are owned clones).
        let parent_sid = match ctx.session_id {
            Some(s) => s,
            None => {
                return Box::pin(async { Err(SpawnFailure::CapabilityDenied) });
            }
        };
        let depth = ctx.agent_identity.as_ref().map(|i| i.depth).unwrap_or(0);
        let cancel = ctx.cancel.clone();
        let this = MultiAgentRuntime {
            registry: Arc::clone(&self.registry),
            store: Arc::clone(&self.store),
            provider: Arc::clone(&self.provider),
            tools: self.tools.clone(),
            config: self.config.clone(),
            worktree_controller: self.worktree_controller.clone(),
            cwd: self.cwd.clone(),
        };
        Box::pin(run_sync_spawn(this, parent_sid, depth, cancel, args))
    }
}

#[allow(clippy::too_many_lines)]
async fn run_sync_spawn(
    this: MultiAgentRuntime,
    parent_sid: SessionId,
    depth: u32,
    cancel: Option<houyicoder_async::CancellationToken>,
    args: SpawnArgs,
) -> Result<SpawnOutcome, SpawnFailure> {
    // The bus + pending-notification path is what makes a background spawn
    // useful; until it lands, refuse so a half-wired async child does not
    // orphan a session the parent is never told about.
    if args.run_in_background {
        return Err(SpawnFailure::CapabilityDenied);
    }
    let def = this
        .registry
        .resolve(&args.subagent_type, &ResolveCtx::default())
        .map_err(|e| match e {
            AgentError::NotFound { .. } => SpawnFailure::UnknownAgent,
            AgentError::PermissionDenied { .. } => SpawnFailure::CapabilityDenied,
        })?;
    let isolation = match args.isolation.as_str() {
        "worktree" => IsolationMode::Worktree,
        _ => IsolationMode::None,
    };
    let base_prompt = match &def.system_prompt {
        PromptSource::Owned(p) => p.clone(),
        PromptSource::InheritParent => this.config.instructions.clone(),
    };
    let child_config = RunnerConfig {
        instructions: child_system_prompt(&base_prompt, &this.cwd, &this.config.model),
        ..this.config.clone()
    };
    let req = SpawnRequest {
        parent_sid,
        parent_store: this.store.clone(),
        provider: this.provider.clone(),
        tools: this.tools.narrow(&def.disallowed_tools),
        config: child_config,
        subagent_type: args.subagent_type.clone(),
        prompt: args.prompt.clone(),
        prompt_summary: args.prompt_summary.clone(),
        depth,
        isolation,
        worktree_controller: this.worktree_controller.clone(),
        run_in_background: false,
        parent_cancel: cancel,
    };
    let handle = spawn_child(req).await.map_err(map_spawn_err)?;
    let child_sid = handle.session;
    let child_str = child_sid.to_string();
    let task = format!(
        "{}\n\n{}",
        child_user_context(&this.cwd, def.omit_project_context),
        args.prompt,
    );
    // A sync child's cancel token is linked to the parent's (spawn_child
    // clones it); on a parent abort, cooperatively cancel the child runner so
    // its tools see the cancel rather than just dropping the future.
    let run_fut = handle.runner.run(child_sid, task);
    let cancel_token = handle.cancel.clone();
    let result: Option<Result<RunResult, _>> = {
        let runner = Arc::clone(&handle.runner);
        tokio::select! {
            biased;
            _ = cancel_token.cancelled() => {
                runner.abort();
                None
            }
            r = run_fut => Some(r),
        }
    };
    if let (Some(cw), Some(ctrl)) = (handle.worktree, this.worktree_controller.as_ref()) {
        drop(ctrl.cleanup_child(cw));
    }
    // The child log holds the partial assistant text a non-final terminal
    // (max_turns, interrupted) or a failed run leaves behind; surface that
    // instead of an empty summary so the parent sees what the child did.
    let child_log = this.store.trajectory_snapshot(child_sid);
    let (status, summary, usage) = match result {
        Some(Ok(r)) => terminal_summary(r, &child_log),
        Some(Err(e)) => {
            // Keep the failure cause even when partial text exists: the
            // parent needs to know the run failed, not just what was said.
            let partial = extract_last_assistant(&child_log);
            let summary = match partial {
                Some(p) => format!("run failed: {e}\n\nPartial output:\n{p}"),
                None => e.to_string(),
            };
            ("failed".to_string(), summary, Usage::default())
        }
        None => (
            "interrupted".to_string(),
            extract_last_assistant(&child_log).unwrap_or_default(),
            Usage::default(),
        ),
    };
    // The boundary is for durability/replay; if the append fails the in-memory
    // result is still valid for this turn, so warn and return the result
    // rather than discarding the child's work.
    if record_subagent_return(
        this.store.as_ref(),
        parent_sid,
        &child_str,
        &status,
        &summary,
        &child_str,
        &usage,
    )
    .await
    .is_err()
    {
        tracing::warn!("subagent return boundary write failed for child {child_str}");
    }
    Ok(SpawnOutcome::sync(child_str, status, summary, usage))
}

fn map_spawn_err(e: SpawnError) -> SpawnFailure {
    match e {
        SpawnError::BudgetExceeded => SpawnFailure::BudgetExceeded,
        SpawnError::CapabilityDenied => SpawnFailure::CapabilityDenied,
        SpawnError::SpawnRecursive => SpawnFailure::Recursive,
        SpawnError::WorktreeFenceNarrowFail => SpawnFailure::FenceFail,
    }
}

/// Map a terminal RunResult to a status label + the summary text the parent
/// sees. FinalOutput carries the answer; other terminals fall back to the
/// last assistant text in the child log (the partial result) so a max-turns
/// or interrupted child is not silently empty.
fn terminal_summary(
    r: RunResult,
    child_log: &[houyicoder_context::TurnEvent],
) -> (String, String, Usage) {
    let usage = r.usage;
    match r.outcome {
        RunOutcome::FinalOutput(t) => ("completed".to_string(), t, usage),
        RunOutcome::MaxTurnsReached { .. } => {
            ("max_turns".to_string(), partial_of(child_log), usage)
        }
        RunOutcome::Interrupted(_) | RunOutcome::Interruption(_) => {
            ("interrupted".to_string(), partial_of(child_log), usage)
        }
        RunOutcome::VerifyFailed(_) => ("verify_failed".to_string(), partial_of(child_log), usage),
        RunOutcome::Handoff(_) => ("handoff".to_string(), partial_of(child_log), usage),
    }
}

/// The last non-empty assistant text in the child log: the partial result a
/// non-final terminal leaves behind.
fn partial_of(child_log: &[houyicoder_context::TurnEvent]) -> String {
    extract_last_assistant(child_log).unwrap_or_default()
}

/// Walk the child log backwards for the last assistant message carrying text;
/// the partial result a non-final terminal leaves behind. None when no
/// assistant message (or none with text) was emitted.
fn extract_last_assistant(events: &[houyicoder_context::TurnEvent]) -> Option<String> {
    use houyicoder_context::TurnEventKind;
    for ev in events.iter().rev() {
        if let TurnEventKind::AssistantMessage { ref text, .. } = ev.kind
            && !text.is_empty()
        {
            return Some(text.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{SessionId, TurnEventKind};
    use houyicoder_core::agent::multi_agent::registry::BuiltInRegistry;
    use houyicoder_core::agent::multi_agent::registry::built_in_all;
    use houyicoder_core::agent::runner_config::RunnerConfig;
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_provider::FakeProvider;
    use houyicoder_session::SessionStore;

    fn runtime_with_text_child(text: &str) -> (MultiAgentRuntime, Arc<SessionStore>, SessionId) {
        let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text(text));
        let registry: Arc<dyn AgentRegistry> =
            Arc::new(BuiltInRegistry::from_agents(built_in_all()));
        let config = RunnerConfig::default();
        let runtime = MultiAgentRuntime::new(
            registry,
            store.clone(),
            provider,
            ToolRegistry::new(),
            config,
            None,
            std::path::PathBuf::from("/tmp"),
        );
        let parent_sid = SessionId::new();
        (runtime, store, parent_sid)
    }

    #[tokio::test]
    async fn test_sync_spawn_drives_terminal() {
        let (runtime, store, parent_sid) = runtime_with_text_child("child answer");
        let ctx = ToolCtx::new("c1").with_session(parent_sid);
        let args = SpawnArgs::new("explore", "find the auth module", "find auth");
        let outcome = runtime.spawn(&ctx, args).await.expect("spawn");
        assert_eq!(outcome.status.as_deref(), Some("completed"));
        assert_eq!(outcome.summary.as_deref(), Some("child answer"));
        // The parent log carries the durable spawn + return boundary pair so
        // replay reconstructs the delegation.
        let events = store.trajectory_snapshot(parent_sid);
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, TurnEventKind::SubagentSpawn { .. })),
            "parent log must record the SubagentSpawn boundary",
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, TurnEventKind::SubagentReturn { .. })),
            "parent log must record the SubagentReturn boundary",
        );
    }

    #[tokio::test]
    async fn test_max_turns_surfaces_partial() {
        // A child that emits text then keeps calling tools past the cap
        // surfaces its last assistant text as the partial result, not an
        // empty summary.
        let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let resp = houyicoder_protocol::llm::CompletionResponse {
            output: vec![
                houyicoder_protocol::llm::OutputItem::Text {
                    text: "halfway findings".into(),
                },
                houyicoder_protocol::llm::OutputItem::ToolCall {
                    id: "call_1".into(),
                    name: "grep".into(),
                    input: serde_json::json!({}),
                },
            ],
            usage: houyicoder_protocol::llm::Usage::default(),
            model: "test".into(),
        };
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![resp]));
        let registry: Arc<dyn AgentRegistry> =
            Arc::new(BuiltInRegistry::from_agents(built_in_all()));
        let config = RunnerConfig {
            max_turns: 1,
            ..RunnerConfig::default()
        };
        let runtime = MultiAgentRuntime::new(
            registry,
            store,
            provider,
            ToolRegistry::new(),
            config,
            None,
            std::path::PathBuf::from("/tmp"),
        );
        let parent_sid = SessionId::new();
        let ctx = ToolCtx::new("c1").with_session(parent_sid);
        let args = SpawnArgs::new("explore", "task", "task");
        let outcome = runtime.spawn(&ctx, args).await.expect("spawn");
        assert_eq!(outcome.status.as_deref(), Some("max_turns"));
        assert_eq!(outcome.summary.as_deref(), Some("halfway findings"));
    }

    #[tokio::test]
    async fn test_sync_spawn_unknown_type() {
        let (runtime, _store, parent_sid) = runtime_with_text_child("x");
        let ctx = ToolCtx::new("c1").with_session(parent_sid);
        let args = SpawnArgs::new("no-such-type", "task", "task");
        let err = runtime.spawn(&ctx, args).await.unwrap_err();
        assert!(matches!(err, SpawnFailure::UnknownAgent));
    }

    #[tokio::test]
    async fn test_async_spawn_refused() {
        let (runtime, _store, parent_sid) = runtime_with_text_child("x");
        let ctx = ToolCtx::new("c1").with_session(parent_sid);
        let mut args = SpawnArgs::new("explore", "task", "task");
        args.run_in_background = true;
        let err = runtime.spawn(&ctx, args).await.unwrap_err();
        assert!(matches!(err, SpawnFailure::CapabilityDenied));
    }
}
