//! Child finalization and background spawn execution.

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use houyicoder_api::agent_event::{EventHandler, RunCompletionStatus, RunLifecycleEvent};
use houyicoder_api::hook_fire::HookFire;
use houyicoder_api::spawn::{SpawnArgs, SpawnFailure, SpawnOutcome};
use houyicoder_async::bus::MessageBus;
use houyicoder_context::{SessionId, SessionLogEntry};
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::multi_agent::bus_types::{
    AgentBus, ChildDescriptor, ChildRunMode, completed_topic,
};
use houyicoder_core::agent::multi_agent::concurrency_gate::AcquireResult;
use houyicoder_core::agent::multi_agent::registry::{IsolationMode, ResolveCtx};
use houyicoder_core::agent::multi_agent::spawn::TriggerSource;
use houyicoder_core::agent::multi_agent::status_publisher::ChildStatusPublisher;
use houyicoder_core::agent::multi_agent::{SpawnRequest, spawn_child};
use houyicoder_protocol::llm::Usage;
use tokio::sync::Notify;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::oneshot::{Sender, channel};
use tokio::task::JoinHandle;

use super::announce_spawn;
use super::drive::drive_child_to_terminal;
use super::fire_subagent_start;
use super::stamp_spawned_by;
use super::{ChildWatch, MultiAgentRuntime, RunningChildren};
use houyicoder_async::CancellationToken;
use houyicoder_core::agent::multi_agent::SpawnError;
use houyicoder_core::agent::multi_agent::child_prompt::child_system_prompt;
use houyicoder_core::agent::multi_agent::registry::AgentDefinition;
use houyicoder_core::agent::multi_agent::registry::AgentError;
use houyicoder_core::agent::multi_agent::registry::PromptSource;
use houyicoder_core::agent::runner_config::RunnerConfig;
use std::panic::resume_unwind;

mod ceiling;
mod execution;
mod handover;
mod request;
use ceiling::await_ceilinged;
use execution::ChildExecution;
use handover::ParentCancelLink;
use request::{ChildRunSpec, child_spawn_request, map_registry_err, map_spawn_err};

pub(super) async fn finalize_child(
    runtime: &MultiAgentRuntime,
    handle: super::ChildHandle,
    parent_sid: SessionId,
    child_sid: SessionId,
    child: ChildDescriptor,
    hook_fire: Option<Arc<dyn HookFire>>,
    task: String,
) -> (String, String, Usage) {
    let child_str = child.agent_id.clone();
    let subagent_type = child.agent_type.clone();
    let cancel_token = handle.cancel.clone();
    // The shared pieces, read once from the runtime the child runs on: the run
    // may publish while this function waits, and the tail has to report
    // against the same bus and store the run was started with.
    let bus = runtime.bus.clone();
    let store = Arc::clone(&runtime.store);
    let worktree_controller = runtime.worktree_controller.clone();
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
    let result = AssertUnwindSafe(drive_child_to_terminal(
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
    let child_tail = store.trajectory_tail(child_sid);
    let (status, summary, payload) = match result {
        Ok(Ok(r)) => {
            let (status, summary) = super::terminal_summary(r, &child_tail);
            (status, summary, None)
        }
        Ok(Err(e)) => {
            let summary = failure_summary(&format!("run failed: {e}"), &child_tail);
            report_failed_terminal(
                bus.as_ref(),
                &child,
                &handle.backgrounded,
                &summary,
                run_published,
            );
            ("failed".to_string(), summary, None)
        }
        Err(payload) => {
            let summary = failure_summary(
                &format!("run panicked: {}", panic_message(payload.as_ref())),
                &child_tail,
            );
            let reason = panic_message(payload.as_ref());
            tracing::error!("child {child_str} run panicked: {reason}");
            report_failed_terminal(
                bus.as_ref(),
                &child,
                &handle.backgrounded,
                &summary,
                run_published,
            );
            ("failed".to_string(), summary, Some(payload))
        }
    };
    // The usage the parent records is read from durable state, not from the
    // run's own result, so a failed, cancelled, or background child reports
    // what it really spent instead of zeroes. See child_subtree_usage.
    let usage = child_subtree_usage(&handle.runner, child_sid);
    if let (Some(cw), Some(ctrl)) = (handle.worktree, worktree_controller.as_ref()) {
        ctrl.cleanup_child(cw).await;
    }
    super::close_child_inbox(bus.as_ref(), &child_str);
    super::fire_subagent_stop(
        hook_fire.as_ref(),
        parent_sid,
        &child_str,
        &subagent_type,
        &status,
        super::extract_last_assistant(&child_tail),
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
        // caller awaiting the task still sees it as the panic it is rather
        // than as an ordinary refusal.
        resume_unwind(payload);
    }
    (status, summary, usage)
}

/// The usage a finished child reports to its parent: its own provider calls
/// plus every descendant it delegated to.
///
/// A child's run result carries only its direct provider usage, so a
/// grandchild's cost would be lost without the second term. The first term
/// reads the child runner's accumulator, the second reads the returns the
/// child's own session folded as they were appended, and each of those returns
/// was itself built this way, so the total covers the whole subtree. Reading
/// durable state also means a failed, cancelled, or background child reports
/// what it really spent rather than zeroes.
///
/// The subtree is read at the moment the child ends. A grandchild still
/// running then has not returned yet, and its return lands after this boundary
/// is written, so its cost reaches no total. A child that finishes before its
/// own children is a gap the delegation model accepts.
pub(super) fn child_subtree_usage(runner: &Runner, child: SessionId) -> Usage {
    let own = runner.status_snapshot().cumulative_usage;
    // The returns come from the store's folded summary rather than from the
    // child's resident rows: the log state holds the newest rows and drops the
    // oldest, so a child whose history outgrew it would report the returns it
    // still held as the whole subtree's cost.
    let descendants = runner.store().subagent_usage(child).to_usage();
    own.saturating_add(&descendants)
}

/// The summary a failed child reports: the reason, plus whatever partial
/// output its transcript holds, so an interrupted run is not silently empty.
fn failure_summary(reason: &str, child_tail: &[SessionLogEntry]) -> String {
    match super::extract_last_assistant(child_tail) {
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
    backgrounded: &Arc<AtomicBool>,
    summary: &str,
    run_published: bool,
) {
    if run_published {
        return;
    }
    let Some(bus) = bus else {
        return;
    };
    // A child the reader handed to the background while it ran reports its
    // ending as a background ending. The parent stopped waiting at that moment,
    // so this notification is the only way it learns the child died; reporting
    // a foreground ending would drop that news, because only a background
    // ending notifies. The flag is read here, at the moment of reporting,
    // because the hand-over can land any time between the start and this arm.
    let mut child = child.clone();
    if backgrounded.load(Ordering::Acquire) {
        child.run_mode = ChildRunMode::Background;
    }
    let publisher = ChildStatusPublisher::new(Arc::clone(bus), child);
    publisher.handle(RunLifecycleEvent::Completed {
        status: RunCompletionStatus::Failed,
        summary: summary.to_string(),
    });
}

/// Start a background child and return once it exists.
///
/// Capacity stays held until the child's run is over. The task owns
/// construction, announcement, the Start hook, and cleanup; the caller only
/// admits the child, starts the task, watches it, and waits for the child id.
/// A caller dropped at any await therefore leaves the child admitted, with its
/// task still running it to a terminal.
pub(super) async fn run_background_spawn(
    this: MultiAgentRuntime,
    parent_sid: SessionId,
    depth: u32,
    hook_fire: Option<Arc<dyn HookFire>>,
    trigger: TriggerSource,
    args: SpawnArgs,
) -> Result<SpawnOutcome, SpawnFailure> {
    let def = this
        .registry
        .resolve(&args.subagent_type, &ResolveCtx::default())
        .map_err(map_registry_err)?;
    let isolation = match args.isolation.as_str() {
        "worktree" => IsolationMode::Worktree,
        _ => IsolationMode::None,
    };
    // A background child is refused outright at the cap: the turn is not
    // waiting on it, so there is nobody to queue behind.
    let permit = match this.gate.try_acquire() {
        AcquireResult::Acquired(p) => p,
        AcquireResult::Rejected => return Err(SpawnFailure::ConcurrencySaturated),
    };
    // The child's token is created here and admitted before any side effect:
    // admitting is the first write, and the child itself only exists inside the
    // task after it. A shutdown landing on any statement below already holds
    // this child's credential.
    let close = CancellationToken::new();
    let Some(admission) = this.admit_child(close) else {
        return Err(SpawnFailure::ShuttingDown);
    };
    let request = child_spawn_request(
        &this,
        parent_sid,
        &args,
        ChildRunSpec {
            trigger,
            depth,
            isolation,
            cancel: admission.close.clone(),
            mode: ChildRunMode::Background,
        },
        &def,
    );
    // The started channel carries the child id back once it exists. The permit,
    // the completion sender, and the admission slot ride the task, so a
    // cancelled caller drops none of them.
    let (started, child_id) = channel::<Result<String, SpawnError>>();
    let running = Arc::clone(&this.running);
    let task = ChildExecution {
        running: Arc::clone(&running),
        runtime: this,
        request,
        hooks: hook_fire,
        parent: parent_sid,
        completion: admission.finished,
        admission: admission.slot,
    }
    .detach(permit, started);
    // Watch under the admitted slot before waiting for the child id, so the
    // shutdown join holds the abort backstop across the construction window.
    // Refused means the slot is gone, either because the shutdown set it aside
    // or because the task ended before this call could watch it. Either way
    // this task sits outside the join, so it is waited on inline here, and the
    // child id is still read for the honest refusal.
    if let Err(ChildWatch::Detached(refused)) =
        attach_watch(running.clone(), admission.slot, ChildWatch::Detached(task))
        && let Err(err) = refused.await
        && err.is_panic()
    {
        tracing::warn!("a background child task panicked at the gate refusal: {err}");
    }
    // Three outcomes: the child id, a construction refusal, or nothing at all.
    // The gate separates the last two: closed means the shutdown ended the
    // task, open means it died during construction. Calling the second a
    // shutdown would tell the model the host is going away when it is not.
    match child_id.await {
        Ok(Ok(child_str)) => Ok(SpawnOutcome::background_started(child_str)),
        Ok(Err(spawn_err)) => Err(map_spawn_err(spawn_err)),
        Err(_recv) => {
            if running.lock().map(|g| g.closing).unwrap_or(true) {
                tracing::warn!("a background child task ended before reporting its child");
                return Err(SpawnFailure::ShuttingDown);
            }
            tracing::error!("a background child task ended before constructing its child");
            Err(SpawnFailure::ConstructionFailed)
        }
    }
}

/// Start a foreground child and return its ending.
///
/// The turn waits on the child, so this is the one shape whose caller can still
/// act on the result. Two things can cut the wait short: the turn being
/// stopped, and the reader handing the child to the background.
pub(super) async fn run_foreground_spawn(
    this: MultiAgentRuntime,
    parent_sid: SessionId,
    depth: u32,
    cancel: Option<CancellationToken>,
    hook_fire: Option<Arc<dyn HookFire>>,
    trigger: TriggerSource,
    args: SpawnArgs,
) -> Result<SpawnOutcome, SpawnFailure> {
    let def = this
        .registry
        .resolve(&args.subagent_type, &ResolveCtx::default())
        .map_err(map_registry_err)?;
    // Refuse a shutdown outright before queueing for a slot. Admitting after
    // the wait is the atomic re-check that also creates the credential.
    if this.is_shutting_down() {
        return Err(SpawnFailure::ShuttingDown);
    }
    let isolation = match args.isolation.as_str() {
        "worktree" => IsolationMode::Worktree,
        _ => IsolationMode::None,
    };
    // A resolved child takes a running slot before any side effect. Saturation
    // rejects with backpressure so the model re-queues next turn; a queued
    // spawn waits here rather than refusing the user's request. The task
    // carries the slot, so it releases when the child ends rather than when
    // this call returns.
    let slot = match this.gate.acquire().await {
        AcquireResult::Acquired(p) => p,
        AcquireResult::Rejected => return Err(SpawnFailure::ConcurrencySaturated),
    };
    // The child gets a lifecycle token of its own, not the turn's. One token
    // held by both cancels in both directions: a child that reached its own
    // ceiling would stop the turn that started it, and a turn the user stops
    // would reach a child the reader has already handed to the background. The
    // turn's stop still reaches the child while the turn waits on it, one way,
    // through ParentCancelLink.
    let close = CancellationToken::new();
    // The slot wait can queue for a while, and a shutdown that closed the gate
    // meanwhile must still refuse: admitting is the first side-effect-bearing
    // write and the single admission point, so None means the gate closed while
    // this call waited.
    let Some(admission) = this.admit_child(close) else {
        return Err(SpawnFailure::ShuttingDown);
    };
    // The turn's stop reaches the child only while the turn waits on it; the
    // link lets go the moment the reader takes the child over.
    let link = ParentCancelLink::new(cancel, admission.close.clone());
    let request = child_spawn_request(
        &this,
        parent_sid,
        &args,
        ChildRunSpec {
            trigger,
            depth,
            isolation,
            cancel: admission.close.clone(),
            mode: ChildRunMode::Foreground,
        },
        &def,
    );
    let running = Arc::clone(&this.running);
    // The reader can take this child over while the turn waits, so the call
    // races the child's ending against that signal. The started channel carries
    // the child id back as soon as the child exists, because the hand-over has
    // to name a child the registry already holds.
    let handover = Arc::new(Notify::new());
    let (started, child_id) = channel::<String>();
    let mut task = ChildExecution {
        running: Arc::clone(&running),
        runtime: this,
        request,
        hooks: hook_fire,
        parent: parent_sid,
        completion: admission.finished,
        admission: admission.slot,
    }
    .supervise(slot, Arc::clone(&handover), started);
    // Watch the task under the admitted slot through the one shared path; the
    // completion signal is already on the slot from the admission. Refused
    // means the slot is gone, either because the shutdown set it aside or
    // because the task ended before this call could watch it, so this task is
    // the call's own to wait on, which it does below on every path anyway.
    let watch = ChildWatch::Awaited {
        abort: task.abort_handle(),
    };
    if let Err(refused) = attach_watch(running, admission.slot, watch) {
        drop(refused);
    }
    // Wait for the child to exist before racing: a hand-over has to name a
    // child the registry already holds, and a construction failure is the
    // task's own result, not something this call can report sooner.
    match child_id.await.ok() {
        Some(child_str) => {
            handover::await_child_or_handover(&mut task, &handover, link, child_str).await
        }
        None => handover::await_child_end(task, link).await,
    }
}

/// The one watch path for both shapes: record how the shutdown join observes an
/// admitted child. Err hands the watch back so the caller waits on the task
/// itself instead of leaving it outside the join.
fn attach_watch(
    running: Arc<Mutex<RunningChildren>>,
    slot: u64,
    watch: ChildWatch,
) -> Result<(), ChildWatch> {
    let Ok(mut g) = running.lock() else {
        return Err(watch);
    };
    g.watch(slot, watch)
}
