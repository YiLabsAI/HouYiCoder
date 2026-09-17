//! Child finalization and background spawn execution.

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use houyicoder_api::agent_event::{EventHandler, RunCompletionStatus, RunLifecycleEvent};
use houyicoder_api::hook_fire::HookFire;
use houyicoder_api::session::SessionLog;
use houyicoder_api::spawn::{SpawnArgs, SpawnFailure, SpawnOutcome};
use houyicoder_async::bus::MessageBus;
use houyicoder_context::{SessionId, SessionLogEntry};
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, ChildDescriptor, ChildRunMode, completed_topic,
};
use houyicoder_core::agent::multi_agent::child_prompt::child_system_prompt;
use houyicoder_core::agent::multi_agent::concurrency_gate::AcquireResult;
use houyicoder_core::agent::multi_agent::registry::{IsolationMode, PromptSource, ResolveCtx};
use houyicoder_core::agent::multi_agent::status_publisher::ChildStatusPublisher;
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
    child: ChildDescriptor,
    hook_fire: Option<Arc<dyn HookFire>>,
    task: String,
) -> (String, String, Usage) {
    let child_str = child.agent_id.clone();
    let subagent_type = child.agent_type.clone();
    let cancel_token = handle.cancel.clone();
    // Subscribed before the run, so a terminal the run publishes is seen here:
    // the child's own terminal keeps the last word, and the failure arms below
    // only report one the run was kept from publishing.
    let mut terminal = bus
        .as_ref()
        .map(|bus| bus.subscribe(&completed_topic(&child_str)));
    // The run can panic — a provider, a tool, a bug in the loop. The unwind
    // must not skip the tail below: it is where the child reaches a terminal,
    // where the parent's return boundary lands, and where the inbox closes and
    // the worktree fence slot is released. Catching here keeps the tail's
    // single copy shared by both spawn paths. The wrap is sound: the runner is
    // not polled again after the catch, so no half-updated state is read.
    let result = AssertUnwindSafe(super::drive::drive_child_to_terminal(
        Arc::clone(&handle.runner),
        child_sid,
        task,
        cancel_token,
        bus.clone(),
        &child_str,
        &subagent_type,
    ))
    .catch_unwind()
    .await;
    // Read once, right after the drive: a terminal the run published is on the
    // receiver by now (the publish is synchronous), so a failure arm below
    // knows whether it owes one.
    let run_published = terminal.as_mut().is_some_and(|rx| rx.try_recv().is_ok());
    let child_log = store.trajectory_snapshot(child_sid);
    let (status, summary, usage, payload) = match result {
        Ok(Ok(r)) => {
            let (status, summary, usage) = super::terminal_summary(r, &child_log);
            (status, summary, usage, None)
        }
        Ok(Err(e)) => {
            let summary = failure_summary(&format!("run failed: {e}"), &child_log);
            report_failed_terminal(bus.as_ref(), &child, &summary, run_published);
            ("failed".to_string(), summary, Usage::default(), None)
        }
        Err(payload) => {
            let summary = failure_summary(
                &format!("run panicked: {}", panic_message(payload.as_ref())),
                &child_log,
            );
            let reason = panic_message(payload.as_ref());
            tracing::error!("child {child_str} run panicked: {reason}");
            report_failed_terminal(bus.as_ref(), &child, &summary, run_published);
            (
                "failed".to_string(),
                summary,
                Usage::default(),
                Some(payload),
            )
        }
    };
    if let (Some(cw), Some(ctrl)) = (handle.worktree, worktree_controller.as_ref()) {
        drop(ctrl.cleanup_child(cw).await);
    }
    super::close_child_inbox(bus.as_ref(), &child_str);
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
    if let Some(payload) = payload {
        // The child is finalized; the panic continues outward from here, so a
        // caller awaiting the driver still sees it as the panic it is rather
        // than as an ordinary refusal.
        std::panic::resume_unwind(payload);
    }
    (status, summary, usage)
}

/// The summary a failed child reports: the reason, plus whatever partial
/// output its transcript holds, so an interrupted run is not silently empty.
fn failure_summary(reason: &str, child_log: &[SessionLogEntry]) -> String {
    match super::extract_last_assistant(child_log) {
        Some(p) => format!("{reason}\n\nPartial output:\n{p}"),
        None => reason.to_string(),
    }
}

/// The text a panic payload carries: the &str or String a panic! with a
/// message produces, a stand-in otherwise.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-text panic payload".to_string()
    }
}

/// Report the terminal a failed run was kept from publishing: the fleet row
/// clears on it, and a background spawn's parent notification fires from it,
/// the same two hops a run that ends on its own goes through. Nothing to do
/// when the run published its own — the fleet row is already clear, and a
/// second report would enqueue a second parent notification, which nothing
/// downstream dedupes. Called before the tail's slower work, so a hanging
/// worktree cleanup cannot hold the row open behind it.
fn report_failed_terminal(
    bus: Option<&Arc<AgentBus>>,
    child: &ChildDescriptor,
    summary: &str,
    run_published: bool,
) {
    if run_published {
        return;
    }
    let Some(bus) = bus else {
        return;
    };
    let publisher = ChildStatusPublisher::new(Arc::clone(bus), child.clone());
    publisher.handle(RunLifecycleEvent::Completed {
        status: RunCompletionStatus::Failed,
        summary: summary.to_string(),
    });
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
    let child = ChildDescriptor::new(
        child_str.clone(),
        args.subagent_type.clone(),
        ChildRunMode::Background,
    );
    // Register the child's live runner so a per-turn abort (the viewed-child
    // Esc path) can reach its turn-cancel token while the async driver runs.
    this.register_child(&child_str, &handle.runner);
    super::announce_spawn(this.bus.as_ref(), child.clone());
    super::fire_subagent_start(
        hook_fire.as_ref(),
        parent_sid,
        &child_str,
        &child.agent_type,
    )
    .await;
    let task = args.prompt.clone();
    let store = this.store.clone();
    let bus = this.bus.clone();
    let worktree_controller = this.worktree_controller.clone();
    let hook_fire_f = hook_fire.clone();
    let parent_sid_f = parent_sid;
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
            child,
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
