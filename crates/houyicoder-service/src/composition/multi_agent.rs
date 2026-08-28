//! The multi-agent runtime: the SpawnHandle impl the agent tool calls.
//! Holds the parent's components (registry, store, provider, tools, config,
//! worktree controller, cwd) and on a sync spawn drives the child runner to
//! a terminal state, records the SubagentReturn boundary, and returns the
//! result the tool projects into its tool_result.

use std::path::PathBuf;
use std::sync::Arc;

use houyicoder_api::hook_fire::HookFire;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_api::spawn::{SpawnArgs, SpawnFailure, SpawnHandle, SpawnOutcome};
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::PFut;
use houyicoder_context::{HookEventKind, HookFirePayload, SessionId};
use houyicoder_core::agent::multi_agent::bus_types::AgentBus;
use houyicoder_core::agent::multi_agent::child_prompt::{child_system_prompt, child_user_context};
use houyicoder_core::agent::multi_agent::concurrency_gate::{AcquireResult, ConcurrencyGate};
use houyicoder_core::agent::multi_agent::registry::{
    AgentError, AgentRegistry, IsolationMode, PromptSource, ResolveCtx,
};
use houyicoder_core::agent::multi_agent::spawn::{ChildHandle, TriggerSource};
use houyicoder_core::agent::multi_agent::{
    SpawnError, SpawnRequest, record_subagent_return, spawn_child,
};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::worktree_controller::WorktreeController;
use houyicoder_core::agent::{RunOutcome, RunResult, ToolRegistry};
use houyicoder_protocol::llm::Usage;

mod drive;
mod spawn_exec;

/// The parent components a child runner is built from: one bundle shared by
/// the composition root, the runtime constructor, and the spawn path, instead
/// of seven loose arguments at each hop.
pub struct MultiAgentDeps {
    pub registry: Arc<dyn AgentRegistry>,
    pub store: Arc<dyn SessionLog>,
    pub provider: Arc<dyn ModelProvider>,
    pub tools: ToolRegistry,
    pub config: RunnerConfig,
    pub worktree_controller: Option<Arc<WorktreeController>>,
    /// The workspace the child's env-block cwd; None falls back to the
    /// process cwd, resolved once at construction.
    pub workspace: Option<PathBuf>,
    /// The in-process message bus shared by parent + all children.
    /// The parent subscribes to child progress/completed topics; the
    /// bus routes parent→child inbox messages. None when async is
    /// not yet wired (sync-only mode).
    pub bus: Option<Arc<AgentBus>>,
}

pub struct MultiAgentRuntime {
    registry: Arc<dyn AgentRegistry>,
    store: Arc<dyn SessionLog>,
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
    config: RunnerConfig,
    worktree_controller: Option<Arc<WorktreeController>>,
    cwd: PathBuf,
    bus: Option<Arc<AgentBus>>,
    /// The per-parent concurrency gate shared across every spawn call from
    /// this runtime. One gate per runtime instance (one parent); the
    /// per-call clone in spawn shares the Arc so all concurrent spawns of
    /// one parent cap against the same slots.
    gate: Arc<ConcurrencyGate>,
}

impl MultiAgentRuntime {
    pub fn new(deps: MultiAgentDeps) -> Self {
        Self {
            registry: deps.registry,
            store: deps.store,
            provider: deps.provider,
            tools: deps.tools,
            config: deps.config,
            worktree_controller: deps.worktree_controller,
            cwd: deps
                .workspace
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
            bus: deps.bus,
            gate: Arc::new(ConcurrencyGate::new(
                ConcurrencyGate::DEFAULT_CAP,
                ConcurrencyGate::DEFAULT_QUEUE_CAP,
            )),
        }
    }

    /// Replace the concurrency gate. Production uses the default gate from
    /// new(); tests inject a small-cap gate to exercise saturation without
    /// spinning up many real children.
    pub(super) fn with_gate(mut self, gate: Arc<ConcurrencyGate>) -> Self {
        self.gate = gate;
        self
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

/// Install the agent directory (registered sub-agent types, minus denied)
/// on the runner's system prompt. Session-stable: the registry is fixed for
/// the session, so the section's bytes do not change across turns.
pub(super) fn wire_agent_directory(
    runner: &houyicoder_core::agent::Runner,
    registry: &dyn AgentRegistry,
    denied: &std::collections::HashSet<String>,
) {
    if let Some(dir) =
        houyicoder_core::agent::multi_agent::registry::agent_directory_section(registry, denied)
    {
        runner.set_agent_directory(dir);
    }
}

/// Build the spawn port the composition root attaches to the runner. The
/// workspace (or the process cwd when none resolved) is the child's env-block
/// cwd. Splits the runtime construction out of the composition root so the
/// root file stays under the size gate.
pub(super) fn build_runtime(deps: MultiAgentDeps) -> Arc<dyn SpawnHandle> {
    Arc::new(MultiAgentRuntime::new(deps))
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
        let hook_fire = ctx.hook_fire.clone();
        let this = MultiAgentRuntime {
            registry: Arc::clone(&self.registry),
            store: Arc::clone(&self.store),
            provider: Arc::clone(&self.provider),
            tools: self.tools.clone(),
            config: self.config.clone(),
            worktree_controller: self.worktree_controller.clone(),
            cwd: self.cwd.clone(),
            bus: self.bus.clone(),
            gate: Arc::clone(&self.gate),
        };
        let trigger = TriggerSource::ModelTool {
            tool_call_id: ctx.call_id.clone(),
        };
        if args.run_in_background {
            Box::pin(spawn_exec::run_async_spawn(
                this, parent_sid, depth, cancel, hook_fire, trigger, args,
            ))
        } else {
            Box::pin(run_sync_spawn(
                this, parent_sid, depth, cancel, hook_fire, trigger, args,
            ))
        }
    }

    /// Route a steering text into a running child's inbox via the shared bus.
    /// The child registered its inbox at spawn; send_inbox errors when the
    /// child is gone (completed + unregistered) or no bus is wired.
    fn send_to_child_inbox(&self, child_id: &str, text: String) -> Result<(), String> {
        use houyicoder_async::bus::MessageBus;
        use houyicoder_core::agent::multi_agent::bus_types::BusMessage;
        match self.bus.as_ref() {
            Some(bus) => bus.send_inbox(child_id, BusMessage::Inbox { text }),
            None => Err("no bus wired".into()),
        }
    }

    /// First-party spawn from a service/hook caller: same spawn pipeline as
    /// the model path, but the trigger is System{hook} so the durable
    /// boundary records a flow-driven origin. A system trigger introduces no
    /// new bypass — the tool-set narrowing (the disallowed list) applies the
    /// same way. The capability-token intersection is not yet wired on either
    /// path (a separate task), so neither path enforces a permission-mode
    /// intersection yet. depth is 0 (the caller must be the root session; a
    /// system spawn from a nested context would reset the recursion counter);
    /// cancel is None (a system spawn is detached — the caller cancels via
    /// the returned child handle); hook_fire is None (no per-call seam from a
    /// non-tool caller; SubagentStart/Stop do not fire for system spawns in
    /// v0 — the first real consumer wires that).
    fn spawn_system(
        &self,
        parent_sid: houyicoder_context::SessionId,
        hook: &str,
        args: SpawnArgs,
    ) -> PFut<'_, Result<SpawnOutcome, SpawnFailure>> {
        let this = MultiAgentRuntime {
            registry: Arc::clone(&self.registry),
            store: Arc::clone(&self.store),
            provider: Arc::clone(&self.provider),
            tools: self.tools.clone(),
            config: self.config.clone(),
            worktree_controller: self.worktree_controller.clone(),
            cwd: self.cwd.clone(),
            bus: self.bus.clone(),
            gate: Arc::clone(&self.gate),
        };
        let trigger = TriggerSource::System {
            hook: hook.to_string(),
        };
        if args.run_in_background {
            Box::pin(spawn_exec::run_async_spawn(
                this, parent_sid, 0, None, None, trigger, args,
            ))
        } else {
            Box::pin(run_sync_spawn(
                this, parent_sid, 0, None, None, trigger, args,
            ))
        }
    }
}

/// Announce a child spawn on the global topic so a watcher (the fleet
/// projector, the completion notification injector) can subscribe to that
/// child's progress and completed topics before the first turn lands. The
/// run_in_background flag lets the notification injector filter detached
/// (async) spawns from sync ones. Fire-and-forget: no watcher, no effect.
fn announce_spawn(
    bus: Option<&Arc<AgentBus>>,
    child_id: &str,
    subagent_type: &str,
    run_in_background: bool,
) {
    use houyicoder_async::bus::MessageBus;
    use houyicoder_core::agent::multi_agent::bus_types::{BusMessage, spawned_topic};
    if let Some(bus) = bus {
        bus.publish(
            spawned_topic(),
            BusMessage::Spawned {
                agent_id: child_id.to_string(),
                subagent_type: subagent_type.to_string(),
                run_in_background,
            },
        );
    }
}

/// Close a child's inbox after its run so a parent's send_inbox errors
/// rather than queueing into a dead receiver. Mirrors announce_spawn.
fn close_child_inbox(bus: Option<&Arc<AgentBus>>, child_id: &str) {
    use houyicoder_async::bus::MessageBus;
    if let Some(bus) = bus {
        bus.unregister_inbox(child_id);
    }
}

/// Fire SubagentStart at the durable spawn boundary. No-op when no hook-fire
/// seam is wired (no registry). The signal lands in the parent log so replay
/// pairs it with the SubagentSpawn / SubagentReturn boundaries.
async fn fire_subagent_start(
    hook_fire: Option<&Arc<dyn HookFire>>,
    parent_sid: SessionId,
    child_str: &str,
    subagent_type: &str,
) {
    if let Some(hf) = hook_fire {
        hf.fire(
            HookEventKind::SubagentStart,
            HookFirePayload::subagent_start(
                parent_sid,
                child_str.to_string(),
                subagent_type.to_string(),
            ),
        )
        .await;
    }
}

/// Fire SubagentStop at the durable return boundary. No-op when no hook-fire
/// seam is wired. status + summary let a hook branch on the terminal kind
/// and inspect the child's last text without reading the transcript.
async fn fire_subagent_stop(
    hook_fire: Option<&Arc<dyn HookFire>>,
    parent_sid: SessionId,
    child_str: &str,
    subagent_type: &str,
    status: &str,
    last_text: Option<String>,
) {
    if let Some(hf) = hook_fire {
        hf.fire(
            HookEventKind::SubagentStop,
            HookFirePayload::subagent_stop(
                parent_sid,
                child_str.to_string(),
                subagent_type.to_string(),
                status.to_string(),
                last_text,
            ),
        )
        .await;
    }
}

async fn run_sync_spawn(
    this: MultiAgentRuntime,
    parent_sid: SessionId,
    depth: u32,
    cancel: Option<houyicoder_async::CancellationToken>,
    hook_fire: Option<Arc<dyn HookFire>>,
    trigger: TriggerSource,
    args: SpawnArgs,
) -> Result<SpawnOutcome, SpawnFailure> {
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
    // Concurrency cap: a resolved (valid type, known isolation) spawn
    // acquires a running slot before any side effect. Saturation rejects with
    // backpressure so the model re-queues next turn; a queued spawn blocks
    // here until a slot frees and is never dropped. The slot is held for the
    // child run and releases on any return path in this scope.
    let _slot = match this.gate.acquire().await {
        AcquireResult::Acquired(p) => p,
        AcquireResult::Rejected => return Err(SpawnFailure::ConcurrencySaturated),
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
        trigger,
        depth,
        isolation,
        worktree_controller: this.worktree_controller.clone(),
        run_in_background: false,
        parent_cancel: cancel,
        bus: this.bus.clone(),
    };
    let handle = spawn_child(req).await.map_err(map_spawn_err)?;
    let child_sid = handle.session;
    let child_str = child_sid.to_string();
    announce_spawn(this.bus.as_ref(), &child_str, &args.subagent_type, false);
    // SubagentStart fires at the durable spawn boundary (child session exists,
    // SubagentSpawn recorded, run not started); pairs with the later
    // SubagentStop across the SubagentSpawn-to-Return span.
    fire_subagent_start(
        hook_fire.as_ref(),
        parent_sid,
        &child_str,
        &args.subagent_type,
    )
    .await;
    let task = format!(
        "{}\n\n{}",
        child_user_context(&this.cwd, def.omit_project_context),
        args.prompt,
    );
    let (status, summary, usage) = spawn_exec::finalize_child(
        handle,
        this.store.clone(),
        this.bus.clone(),
        this.worktree_controller.clone(),
        parent_sid,
        child_sid,
        child_str.clone(),
        args.subagent_type.clone(),
        hook_fire,
        task,
    )
    .await;
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
mod tests;
