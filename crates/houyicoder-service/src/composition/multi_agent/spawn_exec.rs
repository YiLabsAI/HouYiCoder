//! Child finalization and background spawn execution.

use std::sync::Arc;

use houyicoder_api::hook_fire::HookFire;
use houyicoder_api::session::SessionLog;
use houyicoder_api::spawn::{SpawnArgs, SpawnFailure, SpawnOutcome};
use houyicoder_context::SessionId;
use houyicoder_core::agent::multi_agent::bus_types::{AgentBus, ChildDescriptor, ChildRunMode};
use houyicoder_core::agent::multi_agent::child_prompt::child_system_prompt;
use houyicoder_core::agent::multi_agent::concurrency_gate::AcquireResult;
use houyicoder_core::agent::multi_agent::registry::{IsolationMode, PromptSource, ResolveCtx};
use houyicoder_core::agent::multi_agent::{SpawnRequest, spawn_child};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::worktree_controller::WorktreeController;
use houyicoder_protocol::llm::Usage;

use super::MultiAgentRuntime;

#[expect(
    clippy::too_many_arguments,
    reason = "bundles the child handle + parent deps shared by sync + async finalization"
)]
pub(super) async fn finalize_child(
    handle: super::ChildHandle,
    store: Arc<dyn SessionLog>,
    bus: Option<Arc<AgentBus>>,
    worktree_controller: Option<Arc<WorktreeController>>,
    parent_sid: SessionId,
    child_sid: SessionId,
    child_str: String,
    subagent_type: String,
    hook_fire: Option<Arc<dyn HookFire>>,
    task: String,
) -> (String, String, Usage) {
    let cancel_token = handle.cancel.clone();
    let result = super::drive::drive_child_to_terminal(
        Arc::clone(&handle.runner),
        child_sid,
        task,
        cancel_token,
        bus.clone(),
        &child_str,
        &subagent_type,
    )
    .await;
    if let (Some(cw), Some(ctrl)) = (handle.worktree, worktree_controller.as_ref()) {
        drop(ctrl.cleanup_child(cw).await);
    }
    super::close_child_inbox(bus.as_ref(), &child_str);
    let child_log = store.trajectory_snapshot(child_sid);
    let (status, summary, usage) = match result {
        Some(Ok(r)) => super::terminal_summary(r, &child_log),
        Some(Err(e)) => {
            let partial = super::extract_last_assistant(&child_log);
            let summary = match partial {
                Some(p) => format!("run failed: {e}\n\nPartial output:\n{p}"),
                None => e.to_string(),
            };
            ("failed".to_string(), summary, Usage::default())
        }
        None => (
            "interrupted".to_string(),
            super::extract_last_assistant(&child_log).unwrap_or_default(),
            Usage::default(),
        ),
    };
    super::fire_subagent_stop(
        hook_fire.as_ref(),
        parent_sid,
        &child_str,
        &subagent_type,
        &status,
        super::extract_last_assistant(&child_log),
    )
    .await;
    if super::record_subagent_return(
        store.as_ref(),
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
    (status, summary, usage)
}

/// Spawn a background child and return after it starts. Capacity remains held
/// until the detached driver finishes cleanup and publishes completion.
pub(super) async fn run_background_spawn(
    this: MultiAgentRuntime,
    parent_sid: SessionId,
    depth: u32,
    cancel: Option<houyicoder_async::CancellationToken>,
    hook_fire: Option<Arc<dyn HookFire>>,
    trigger: super::TriggerSource,
    args: SpawnArgs,
) -> Result<SpawnOutcome, SpawnFailure> {
    let def = this
        .registry
        .resolve(&args.subagent_type, &ResolveCtx::default())
        .map_err(super::map_registry_err)?;
    let isolation = match args.isolation.as_str() {
        "worktree" => IsolationMode::Worktree,
        _ => IsolationMode::None,
    };
    // Background spawns reject saturation without blocking the parent turn.
    let permit = match this.gate.try_acquire() {
        AcquireResult::Acquired(p) => p,
        AcquireResult::Rejected => return Err(SpawnFailure::ConcurrencySaturated),
    };
    let base_prompt = match &def.system_prompt {
        PromptSource::Owned(p) => p.clone(),
        PromptSource::InheritParent => this.config.instructions.clone(),
    };
    let child_config = RunnerConfig {
        instructions: child_system_prompt(
            &base_prompt,
            &this.cwd,
            &this.config.model,
            def.omit_project_context,
        ),
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
        run_mode: ChildRunMode::Background,
        parent_cancel: cancel,
        bus: this.bus.clone(),
    };
    let handle = spawn_child(req).await.map_err(super::map_spawn_err)?;
    let child_sid = handle.session;
    let child_str = child_sid.to_string();
    // Register the child's live runner so a per-turn abort (the viewed-child
    // Esc path) can reach its turn-cancel token while the async driver runs.
    this.register_child(&child_str, &handle.runner);
    super::announce_spawn(
        this.bus.as_ref(),
        ChildDescriptor::new(
            child_str.clone(),
            args.subagent_type.clone(),
            ChildRunMode::Background,
        ),
    );
    super::fire_subagent_start(
        hook_fire.as_ref(),
        parent_sid,
        &child_str,
        &args.subagent_type,
    )
    .await;
    let task = args.prompt.clone();
    let store = this.store.clone();
    let bus = this.bus.clone();
    let worktree_controller = this.worktree_controller.clone();
    let subagent_type = args.subagent_type.clone();
    let hook_fire_f = hook_fire.clone();
    let parent_sid_f = parent_sid;
    let child_str_f = child_str.clone();
    let child_str_stamp = child_str.clone();
    let descriptor_store_f = this.descriptor_store.clone();
    let subagent_type_f = args.subagent_type.clone();
    tokio::spawn(async move {
        // The permit releases here (end of the driver) so the slot frees when
        // the child completes — the async run cannot outlive the cap. The
        // result reaches the parent via the bus completed publish; the return
        // is dropped (cleanup, Stop, Return boundary ran in finalize_child).
        let _permit = permit;
        let _outcome = finalize_child(
            handle,
            store,
            bus,
            worktree_controller,
            parent_sid_f,
            child_sid,
            child_str_f,
            subagent_type,
            hook_fire_f,
            task,
        )
        .await;
        super::stamp_spawned_by(
            &descriptor_store_f,
            child_sid,
            parent_sid_f,
            &subagent_type_f,
            &child_str_stamp,
        );
    });
    Ok(SpawnOutcome::background_started(child_str))
}
