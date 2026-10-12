//! Child finalization and background spawn execution.

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use houyicoder_api::agent_event::{EventHandler, RunCompletionStatus, RunLifecycleEvent};
use houyicoder_api::hook_fire::HookFire;
use houyicoder_api::session::SessionLog;
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
use houyicoder_core::agent::multi_agent::{SpawnRequest, aggregate_subagent_usage, spawn_child};
use houyicoder_core::agent::worktree_controller::WorktreeController;
use houyicoder_protocol::llm::Usage;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::oneshot::{Sender, channel};
use tokio::task::JoinHandle;

use super::announce_spawn;
use super::fire_subagent_start;
use super::stamp_spawned_by;
use super::{DriverEntry, DriverRegistry, MultiAgentRuntime};
use houyicoder_async::CancellationToken;
use houyicoder_core::agent::multi_agent::SpawnError;
use houyicoder_core::agent::multi_agent::child_prompt::child_system_prompt;
use houyicoder_core::agent::multi_agent::registry::AgentDefinition;
use houyicoder_core::agent::multi_agent::registry::AgentError;
use houyicoder_core::agent::multi_agent::registry::PromptSource;
use houyicoder_core::agent::runner_config::RunnerConfig;

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
    let (status, summary, payload) = match result {
        Ok(Ok(r)) => {
            let (status, summary) = super::terminal_summary(r, &child_log);
            (status, summary, None)
        }
        Ok(Err(e)) => {
            let summary = failure_summary(&format!("run failed: {e}"), &child_log);
            report_failed_terminal(bus.as_ref(), &child, &summary, run_published);
            ("failed".to_string(), summary, None)
        }
        Err(payload) => {
            let summary = failure_summary(
                &format!("run panicked: {}", panic_message(payload.as_ref())),
                &child_log,
            );
            let reason = panic_message(payload.as_ref());
            tracing::error!("child {child_str} run panicked: {reason}");
            report_failed_terminal(bus.as_ref(), &child, &summary, run_published);
            ("failed".to_string(), summary, Some(payload))
        }
    };
    // The usage the parent records is read from durable state, not from the
    // run's own result, so a failed, cancelled, or background child reports
    // what it really spent instead of zeroes. See child_subtree_usage.
    let usage = child_subtree_usage(&handle.runner, &child_log);
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

/// The usage a finished child reports to its parent: its own provider calls
/// plus every descendant it delegated to.
///
/// A child's run result carries only its direct provider usage, so a
/// grandchild's cost would be lost without the second term. The first term
/// reads the child runner's accumulator, the second sums the returns the
/// child's own log holds, and each of those returns was itself built this way,
/// so the total covers the whole subtree. Reading durable state also means a
/// failed, cancelled, or background child reports what it really spent rather
/// than zeroes.
///
/// The subtree is read at the moment the child ends. A grandchild still
/// running then is not in the child's log yet, and its return lands after this
/// boundary is written, so its cost reaches no total. A child that finishes
/// before its own children is a gap the delegation model accepts.
pub(super) fn child_subtree_usage(runner: &Runner, child_log: &[SessionLogEntry]) -> Usage {
    let own = runner.status_snapshot().cumulative_usage;
    let descendants = aggregate_subagent_usage(child_log);
    own.saturating_add(&descendants.to_usage())
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
/// until the detached driver finishes cleanup and publishes completion. The
/// driver owns construction, announcement, the Start hook, and cleanup; the
/// caller only admits the credential, starts the driver, attaches it, and
/// awaits the child id back, so a caller dropped at any await leaves the child
/// registered with its driver still driving it to a terminal.
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
    // Background spawns reject saturation without blocking the parent turn.
    let permit = match this.gate.try_acquire() {
        AcquireResult::Acquired(p) => p,
        AcquireResult::Rejected => return Err(SpawnFailure::ConcurrencySaturated),
    };
    // The close token is created here and admitted before any side effect: the
    // reserve below is the first registry write, and the child (the first real
    // side effect) only exists inside the driver after it. A shutdown landing
    // on any statement below already holds this child's credential.
    let close = CancellationToken::new();
    let Some(reservation) = this.reserve_driver(close) else {
        return Err(SpawnFailure::ShuttingDown);
    };
    let req = child_spawn_request(
        &this,
        parent_sid,
        &args,
        trigger,
        depth,
        isolation,
        reservation.close.clone(),
        ChildRunMode::Background,
        &def,
    );
    // The driver owns construction; the ready channel carries the child id
    // back once it exists. The permit, the completion Sender, and the
    // registry removal key ride the driver, so a cancelled caller drops none
    // of them.
    let (ready_tx, ready) = channel::<Result<String, SpawnError>>();
    let drivers = Arc::clone(&this.child_drivers);
    let gate = Arc::clone(&this.child_drivers);
    let driver = background_driver(
        this,
        permit,
        req,
        hook_fire,
        parent_sid,
        reservation.done_tx,
        drivers,
        reservation.key,
        ready_tx,
    );
    // Attach under the reserved key before awaiting ready, so the shutdown
    // join holds the abort backstop across the construction window. Refused
    // means the gate closed after the reserve: the join drained the slot
    // (still unattached), so this driver sits outside it — await it inline so
    // its terminal and cleanups land before this call returns, then still read
    // ready for the honest refusal.
    if let Err(DriverEntry::Background(refused)) = attach_driver(
        gate.clone(),
        reservation.key,
        DriverEntry::Background(driver),
    ) && let Err(err) = refused.await
        && err.is_panic()
    {
        tracing::warn!("background child driver panicked at gate refusal: {err}");
    }
    // Three ready outcomes: the child id, a construction refusal (surface it
    // as the failure it is), or nothing — the driver ended before reporting.
    // The gate disambiguates the nothing case: a closed gate means the shutdown
    // abort ended the driver, an open gate means it died abnormally during
    // construction (a panic). Reporting a shutdown for the latter would tell
    // the model the host is going away when it is not; report the honest
    // failed start and keep the diagnosis loud.
    match ready.await {
        Ok(Ok(child_str)) => Ok(SpawnOutcome::background_started(child_str)),
        Ok(Err(spawn_err)) => Err(map_spawn_err(spawn_err)),
        Err(_recv) => {
            if gate.lock().map(|g| g.closing).unwrap_or(true) {
                tracing::warn!("background child driver aborted before reporting its child");
                return Err(SpawnFailure::ShuttingDown);
            }
            tracing::error!("background child driver ended before constructing its child");
            Err(SpawnFailure::ConstructionFailed)
        }
    }
}

/// The one attach path for both run modes: insert under the reserved key
/// unless the gate closed. Err hands the entry back so the caller joins the
/// driver itself instead of leaving it outside the shutdown join.
fn attach_driver(
    drivers: Arc<Mutex<DriverRegistry>>,
    key: u64,
    entry: DriverEntry,
) -> Result<(), DriverEntry> {
    let Ok(mut g) = drivers.lock() else {
        return Err(entry);
    };
    g.attach(key, entry)
}

/// The detached background driver: construct the child, stamp its provenance,
/// register it, announce it, fire the Start hook, drive it to its finalize
/// tail, then remove the registry entry. The permit, the completion Sender,
/// and the registry key ride the driver so a cancelled caller drops none of
/// them; the child id returns through the ready channel the moment
/// construction succeeds.
#[expect(
    clippy::too_many_arguments,
    reason = "carries the parent deps + the registry identity + the ready the driver owns"
)]
fn background_driver(
    this: MultiAgentRuntime,
    permit: OwnedSemaphorePermit,
    req: SpawnRequest,
    hook_fire: Option<Arc<dyn HookFire>>,
    parent_sid: SessionId,
    done_tx: Sender<()>,
    drivers: Arc<Mutex<DriverRegistry>>,
    driver_key: u64,
    ready_tx: Sender<Result<String, SpawnError>>,
) -> JoinHandle<()> {
    let subagent_type = req.subagent_type.clone();
    let task = req.prompt.clone();
    tokio::spawn(async move {
        // The completion signal fires when this driver ends, however it ends:
        // the guard rides the whole task, so the shutdown join's wait on it
        // observes the finalize tail too. The permit releases at the same
        // point, so the slot frees when the child completes. The result
        // reaches the parent via the bus completed publish; the return is
        // dropped (cleanup, Stop, Return boundary ran in finalize_child).
        let _done = done_tx;
        let _permit = permit;
        // spawn_child is the first side effect; a refusal here is reported
        // through the ready channel so the caller surfaces the honest reason
        // rather than a fabricated child.
        let handle = match spawn_child(req).await {
            Ok(h) => h,
            Err(e) => {
                drop(ready_tx.send(Err(e)));
                return;
            }
        };
        let child_sid = handle.session;
        let child_str = child_sid.to_string();
        // The child's provenance lands at the boundary, before its run: the
        // delegation record it opens with is its first durable append, so
        // nothing that counts or lists the user's sessions can read a running
        // child as one of theirs.
        stamp_spawned_by(
            &this.descriptor_store,
            child_sid,
            parent_sid,
            &subagent_type,
            &child_str,
        );
        let child = ChildDescriptor::new(
            child_str.clone(),
            subagent_type.clone(),
            ChildRunMode::Background,
        );
        // Register the live runner for a per-turn abort (the viewed-child Esc
        // path). The lifecycle token is registered alongside so a shutdown
        // stop reaches a child whose run has not started.
        this.register_child(&child_str, &handle.runner, handle.cancel.clone());
        announce_spawn(this.bus.as_ref(), child.clone());
        // Construction succeeded; report the child id before the slower Start
        // hook and run so the caller returns promptly after the child is real.
        drop(ready_tx.send(Ok(child_str.clone())));
        // The Start hook dispatch can wait on a command hook. It runs in the
        // driver, not in the caller, because a caller dropped at that wait
        // (Esc mid-turn) would leave the child announced and registered with
        // nothing driving it to a terminal.
        fire_subagent_start(hook_fire.as_ref(), parent_sid, &child_str, &subagent_type).await;
        let _outcome = finalize_child(
            handle,
            this.store.clone(),
            this.bus.clone(),
            this.worktree_controller.clone(),
            parent_sid,
            child_sid,
            child,
            hook_fire,
            task,
        )
        .await;
        // The child is finalized; drop its driver entry so a later shutdown
        // join waits only for children still running.
        if let Ok(mut g) = drivers.lock() {
            g.remove(driver_key);
        }
    })
}
pub(super) async fn run_foreground_spawn(
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
        .map_err(map_registry_err)?;
    // Fast reject before queueing for a slot: a shutdown in progress refuses
    // the spawn outright. The reserve after the slot wait is the atomic
    // re-check that also creates the admission credential.
    if this.is_shutting_down() {
        return Err(SpawnFailure::ShuttingDown);
    }
    let isolation = match args.isolation.as_str() {
        "worktree" => IsolationMode::Worktree,
        _ => IsolationMode::None,
    };
    // Concurrency cap: a resolved (valid type, known isolation) spawn
    // acquires a running slot before any side effect. Saturation rejects with
    // backpressure so the model re-queues next turn; a queued spawn waits here
    // for a slot rather than refusing the user's request. The driver below
    // carries the slot, so it releases when the child ends rather than when
    // this call returns — a stop releases the slot when the child finishes
    // winding down.
    let slot = match this.gate.acquire().await {
        AcquireResult::Acquired(p) => p,
        AcquireResult::Rejected => return Err(SpawnFailure::ConcurrencySaturated),
    };
    // The close token links to the parent token: a parent abort reaches the
    // child. Creating it here and passing the same clone to both the reserve
    // and the spawn request keeps the shutdown credential and the child handle
    // on one token.
    let close = cancel.clone().unwrap_or_default();
    // The slot wait can queue for a while; a shutdown that closed the gate
    // while this call waited must still refuse, or the child would be created
    // after the drain with no join covering it. The reserve is the first
    // side-effect-bearing write and the single admission point: it creates the
    // close + done credential before any child exists, and None means the gate
    // closed while this call waited.
    let Some(reservation) = this.reserve_driver(close) else {
        return Err(SpawnFailure::ShuttingDown);
    };
    let req = child_spawn_request(
        &this,
        parent_sid,
        &args,
        trigger,
        depth,
        isolation,
        reservation.close.clone(),
        ChildRunMode::Foreground,
        &def,
    );
    let task = req.prompt.clone();
    let drivers = Arc::clone(&this.child_drivers);
    let driver = foreground_driver(
        this,
        req,
        slot,
        hook_fire,
        parent_sid,
        reservation.done_tx,
        task,
    );
    // Attach the driver's abort entry under the reserved key through the one
    // shared attach path; the completion signal is already on the slot from
    // the reserve. Refused means the gate closed while the child ran: the
    // shutdown join has already drained, so this driver is the spawn call's
    // own to await (the driver.await below), which it does on every path
    // anyway.
    let entry = DriverEntry::Foreground {
        abort: driver.abort_handle(),
    };
    if let Err(refused) = attach_driver(drivers, reservation.key, entry) {
        drop(refused);
    }
    match driver.await {
        Ok(outcome) => outcome,
        // A panic inside the driver would otherwise reach the model as an
        // ordinary refusal, so surface it as the panic it is.
        Err(join) => std::panic::resume_unwind(join.into_panic()),
    }
}

/// The foreground child's driver task: the child runs on it, but the spawn
/// call awaits the handle, so the driver is registered for the shutdown join
/// through its completion signal rather than the handle itself. The driver
/// owns the running slot and the child's bookkeeping, so a stop reaches the
/// child as an interrupt it can finish from rather than as a dropped future;
/// a child parked in work that never reads its cancel token holds the slot
/// and the row until that work returns.
fn foreground_driver(
    this: MultiAgentRuntime,
    req: SpawnRequest,
    slot: OwnedSemaphorePermit,
    hook_fire: Option<Arc<dyn HookFire>>,
    parent_sid: SessionId,
    done_tx: Sender<()>,
    task: String,
) -> JoinHandle<Result<SpawnOutcome, SpawnFailure>> {
    let subagent_type = req.subagent_type.clone();
    tokio::spawn(async move {
        // The completion signal fires when this driver ends, however it ends:
        // the guard rides the whole task, so the shutdown join's wait on it
        // observes the finalize tail too.
        let _done = done_tx;
        let _slot = slot;
        let handle = spawn_child(req).await.map_err(map_spawn_err)?;
        let child_sid = handle.session;
        let child_str = child_sid.to_string();
        // The child's provenance lands at the boundary, before its run: the
        // delegation record it opens with is its first durable append, so
        // nothing that counts or lists the user's sessions can read a running
        // child as one of theirs.
        stamp_spawned_by(
            &this.descriptor_store,
            child_sid,
            parent_sid,
            &subagent_type,
            &child_str,
        );
        let child = ChildDescriptor::new(
            child_str.clone(),
            subagent_type.clone(),
            ChildRunMode::Foreground,
        );
        // Register the live runner for a per-turn abort (moot for sync — the
        // parent blocks on the tool call + cannot be viewing the child mid-run
        // — but the registry is shared so the async path's contract holds
        // uniformly). The lifecycle token is registered alongside so a
        // shutdown stop reaches a child whose run has not started.
        this.register_child(&child_str, &handle.runner, handle.cancel.clone());
        announce_spawn(this.bus.as_ref(), child.clone());
        // SubagentStart fires at the durable spawn boundary (child session
        // exists, SubagentSpawn recorded, run not started); pairs with the
        // later SubagentStop across the SubagentSpawn-to-Return span.
        fire_subagent_start(hook_fire.as_ref(), parent_sid, &child_str, &subagent_type).await;
        let (status, summary, usage) = finalize_child(
            handle,
            this.store.clone(),
            this.bus.clone(),
            this.worktree_controller.clone(),
            parent_sid,
            child_sid,
            child,
            hook_fire,
            task,
        )
        .await;
        Ok(SpawnOutcome::foreground(child_str, status, summary, usage))
    })
}

fn map_registry_err(error: AgentError) -> SpawnFailure {
    match error {
        AgentError::NotFound { .. } => SpawnFailure::UnknownAgent,
        AgentError::PermissionDenied { .. } => SpawnFailure::CapabilityDenied,
    }
}

/// Build the child's spawn request from the resolved definition: the child
/// prompt, the narrowed tools, and the parent's shared components. One
/// builder for both run modes so the foreground and background paths cannot
/// drift apart on what a child carries.
#[expect(
    clippy::too_many_arguments,
    reason = "the spawn request has this many independent parts; bundling them would duplicate SpawnRequest"
)]
fn child_spawn_request(
    this: &MultiAgentRuntime,
    parent_sid: SessionId,
    args: &SpawnArgs,
    trigger: TriggerSource,
    depth: u32,
    isolation: IsolationMode,
    child_cancel: CancellationToken,
    run_mode: ChildRunMode,
    def: &AgentDefinition,
) -> SpawnRequest {
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
    SpawnRequest {
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
        run_mode,
        child_cancel,
        bus: this.bus.clone(),
    }
}

fn map_spawn_err(e: SpawnError) -> SpawnFailure {
    match e {
        SpawnError::BudgetExceeded => SpawnFailure::BudgetExceeded,
        SpawnError::CapabilityDenied => SpawnFailure::CapabilityDenied,
        SpawnError::SpawnRecursive => SpawnFailure::Recursive,
        SpawnError::BoundaryWriteFailed => SpawnFailure::BoundaryWriteFailed,
        SpawnError::WorktreeFenceNarrowFail => SpawnFailure::FenceFail,
    }
}
