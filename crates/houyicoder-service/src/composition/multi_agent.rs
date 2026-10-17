//! The multi-agent runtime: the SpawnHandle impl the agent tool calls.
//! Holds the parent's components (registry, store, provider, tools, config,
//! worktree controller, cwd) and on a foreground spawn drives the child runner to
//! a terminal state, records the SubagentReturn boundary, and returns the
//! result the tool maps into its tool_result.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use houyicoder_api::hook_fire::HookFire;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_api::spawn::{SpawnArgs, SpawnFailure, SpawnHandle, SpawnOutcome};
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::CancellationToken;
use houyicoder_async::PFut;
use houyicoder_async::bus::MessageBus;
use houyicoder_context::SessionEvent;
use houyicoder_context::{
    DescriptorUpdate, HookEventKind, HookFirePayload, SessionDescriptor, SessionDescriptorStore,
    SessionId, SessionProvenance,
};
use houyicoder_core::agent::multi_agent::bus_types::BusMessage;
use houyicoder_core::agent::multi_agent::bus_types::spawned_topic;
use houyicoder_core::agent::multi_agent::bus_types::{AgentBus, ChildDescriptor};
use houyicoder_core::agent::multi_agent::concurrency_gate::ConcurrencyGate;
use houyicoder_core::agent::multi_agent::record_subagent_return;
use houyicoder_core::agent::multi_agent::registry::AgentRegistry;
use houyicoder_core::agent::multi_agent::registry::BuiltInRegistry;
use houyicoder_core::agent::multi_agent::registry::built_in_all;
use houyicoder_core::agent::multi_agent::registry::delegatable_agents;
use houyicoder_core::agent::multi_agent::spawn::{ChildHandle, TriggerSource};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::worktree_controller::WorktreeController;
use houyicoder_core::agent::{RunOutcome, RunResult, Runner, ToolRegistry};
use std::env::current_dir;
use std::mem::take;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::oneshot::{Receiver, Sender, channel};
use tokio::task::AbortHandle;
use tokio::task::JoinHandle;
use tokio::time::timeout;

mod drive;
mod spawn_exec;

/// Per-driver budget the shutdown join gives a child's finalize tail before
/// aborting it. Generous for real cleanup work (worktree removal, boundary
/// writes) so a healthy tail finishes inside it; the abort backstop keeps a
/// wedged driver from holding the lock forever. Under test the budget
/// shrinks so the backstop path is exercisable in milliseconds.
#[cfg(not(test))]
const CHILD_JOIN_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const CHILD_JOIN_TIMEOUT: Duration = Duration::from_millis(200);

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
    /// Descriptor store used to stamp child provenance so the resume picker can
    /// filter subagent sessions out.
    pub descriptor_store: Option<Arc<dyn SessionDescriptorStore>>,
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
    /// Live child runners keyed by child session id, so a per-turn abort
    /// (the viewed-child Esc path) can reach the child's turn-cancel token
    /// without holding the Arc (the async driver moves the Arc into its
    /// task; a Weak upgrades only while the child is still running). Stale
    /// entries (child completed + dropped) are pruned on a failed upgrade.
    children: Arc<Mutex<HashMap<String, ChildEntry>>>,
    /// The detached driver tasks (background and foreground), plus the
    /// closing gate. The gate and the registry share one lock so a shutdown
    /// that closes the gate and drains the drivers is atomic against a spawn
    /// registering a driver: registration either lands before the drain (the
    /// driver is joined) or is refused (the caller joins it inline), never
    /// silently after.
    child_drivers: Arc<Mutex<DriverRegistry>>,
    /// Descriptor store for stamping SpawnedBy provenance on child sessions.
    descriptor_store: Option<Arc<dyn SessionDescriptorStore>>,
}

/// One live child: its runner (Weak, so a finished child drops the entry's
/// reach) and its lifecycle cancel token. The token is level-triggered and
/// exists from the spawn boundary, before the run's first poll mints the
/// turn token — so a shutdown stop issued while the driver has not started
/// the run still reaches the child instead of firing at a runner with no
/// token to cancel.
struct ChildEntry {
    runner: Weak<Runner>,
    cancel: CancellationToken,
}

/// How the shutdown join observes one driver. A background driver's handle
/// is owned solely by the registry. A foreground driver's handle stays with
/// the caller (it awaits the child's outcome), so the registry holds only
/// the abort handle; the completion signal lives on the slot, shared by both
/// modes.
pub(super) enum DriverEntry {
    Background(JoinHandle<()>),
    Foreground { abort: AbortHandle },
}

/// One admitted driver's shutdown credential: the child's lifecycle cancel
/// token (the exact token stored on the child handle) plus the completion
/// signal the join waits on. The entry only exists once attached; before
/// that the close and done pair already covers the child, so a shutdown
/// landing between admission and attach still cancels it and waits for its
/// driver to end.
struct DriverSlot {
    close: CancellationToken,
    done: Receiver<()>,
    entry: Option<DriverEntry>,
}

/// What reserve hands back to the caller: the reserved key, the close token
/// to pass to spawn as the child's cancel, and the completion Sender to move
/// into the driver task. done_tx spends only the caller's consecutive
/// synchronous statements before entering the driver, so a cancelled caller
/// never drops it.
struct Reservation {
    key: u64,
    close: CancellationToken,
    done_tx: Sender<()>,
}

/// The driver registry with its closing gate. Keys are reserved before the
/// driver task is built (the task needs its key to remove itself) and
/// attached after; both steps refuse once the gate is closed.
#[derive(Default)]
struct DriverRegistry {
    closing: bool,
    next_key: u64,
    drivers: HashMap<u64, DriverSlot>,
    /// The drivers the close step drained, waiting for the join to take.
    draining: Vec<(u64, DriverSlot)>,
}

impl DriverRegistry {
    /// Reserve a driver's admission credential before its first side effect.
    /// Creates the completion channel so close and done are both live from
    /// this moment, before the child exists. None once the gate is closed: a
    /// shutdown in progress refuses new children at the door.
    fn reserve(&mut self, close: CancellationToken) -> Option<Reservation> {
        if self.closing {
            return None;
        }
        let key = self.next_key;
        self.next_key += 1;
        let (done_tx, done) = channel::<()>();
        self.drivers.insert(
            key,
            DriverSlot {
                close: close.clone(),
                done,
                entry: None,
            },
        );
        Some(Reservation {
            key,
            close,
            done_tx,
        })
    }

    /// Attach a spawned driver under its reserved key. Err hands the entry
    /// back once the slot is gone (a shutdown drained it): the caller must
    /// then join the driver itself. The slot's presence is the whole answer,
    /// so this no longer reads closing separately.
    fn attach(&mut self, key: u64, entry: DriverEntry) -> Result<(), DriverEntry> {
        match self.drivers.get_mut(&key) {
            Some(slot) => {
                slot.entry = Some(entry);
                Ok(())
            }
            None => Err(entry),
        }
    }

    /// The driver finished; drop its entry so a later join waits only for
    /// still-running drivers.
    fn remove(&mut self, key: u64) {
        self.drivers.remove(&key);
    }

    /// Close the gate and drain the registered drivers into the taken state,
    /// where they wait for take_drained. The caller holds this lock across
    /// the children cancel pass, which is what makes a registration racing
    /// the shutdown see either a cancelled token or a closed gate — never
    /// neither. The drain fills only when empty, so a repeated close cannot
    /// discard a prior drain the join has not taken yet; the single-close
    /// consumer stays the boundary.
    fn close_and_drain_into(&mut self) {
        self.closing = true;
        if self.draining.is_empty() {
            self.draining = take(&mut self.drivers).into_iter().collect();
        }
    }

    /// Take the drivers the close step drained. Empty when the gate never
    /// closed (an empty registry at shutdown) or after a prior take.
    fn take_drained(&mut self) -> Vec<(u64, DriverSlot)> {
        take(&mut self.draining).into_iter().collect()
    }
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
                .unwrap_or_else(|| current_dir().unwrap_or_default()),
            bus: deps.bus,
            gate: Arc::new(ConcurrencyGate::new(
                ConcurrencyGate::DEFAULT_CAP,
                ConcurrencyGate::DEFAULT_QUEUE_CAP,
            )),
            children: Arc::new(Mutex::new(HashMap::new())),
            child_drivers: Arc::new(Mutex::new(DriverRegistry::default())),
            descriptor_store: deps.descriptor_store,
        }
    }

    /// Register a live child runner so a later per-turn abort can reach it.
    /// The entry's runner Weak drops to nothing when the child's Arc is
    /// released (the driver task ends); cancel_child_turn prunes the entry
    /// it queries. To keep a long-lived parent's registry from accumulating
    /// stale entries for completed children that were never Esc'd, sweep
    /// dead entries on each register (amortized: one sweep per spawn, not
    /// per poll).
    pub(super) fn register_child(
        &self,
        child_id: &str,
        runner: &Arc<Runner>,
        cancel: CancellationToken,
    ) {
        if let Ok(mut g) = self.children.lock() {
            g.retain(|_, entry| entry.runner.upgrade().is_some());
            g.insert(
                child_id.to_string(),
                ChildEntry {
                    runner: Arc::downgrade(runner),
                    cancel,
                },
            );
        }
    }

    /// Reserve a driver's admission credential before its first side effect.
    /// None once shutdown has closed the gate: the spawn is refused instead
    /// of creating a child the join would miss. A poisoned lock fails the
    /// reserve, which the caller reads as a refusal before any child exists.
    fn reserve_driver(&self, close: CancellationToken) -> Option<Reservation> {
        self.child_drivers
            .lock()
            .ok()
            .and_then(|mut g| g.reserve(close))
    }

    /// Whether shutdown has closed the gate. A cheap read the foreground path
    /// takes before queueing for a concurrency slot; a poisoned lock reads as
    /// true (fail closed), and the later reserve still refuses atomically.
    fn is_shutting_down(&self) -> bool {
        self.child_drivers.lock().map(|g| g.closing).unwrap_or(true)
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
    Arc::new(BuiltInRegistry::from_agents(built_in_all()))
}

/// Install the sub-agent types the model may delegate to (registered, minus
/// denied) on the runner, so the system prompt can announce them and the
/// /agents command can list them. Session-stable: the registry is fixed for
/// the session, so the prompt's bytes do not change across turns.
pub(super) fn install_agent_types(
    runner: &Runner,
    registry: &dyn AgentRegistry,
    denied: &HashSet<String>,
) {
    runner.set_agent_types(delegatable_agents(registry, denied));
}

/// Build the child-spawn port attached to the parent runner.
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
            children: Arc::clone(&self.children),
            child_drivers: Arc::clone(&self.child_drivers),
            descriptor_store: self.descriptor_store.clone(),
        };
        let trigger = TriggerSource::ModelTool {
            tool_call_id: ctx.call_id.clone(),
        };
        if args.run_in_background {
            Box::pin(spawn_exec::run_background_spawn(
                this, parent_sid, depth, hook_fire, trigger, args,
            ))
        } else {
            Box::pin(spawn_exec::run_foreground_spawn(
                this, parent_sid, depth, cancel, hook_fire, trigger, args,
            ))
        }
    }

    /// Route a steering text into a running child's inbox via the shared bus.
    /// The child registered its inbox at spawn; send_inbox errors when the
    /// child is gone (completed + unregistered) or no bus is attached.
    fn send_to_child_inbox(&self, child_id: &str, text: String) -> Result<(), String> {
        match self.bus.as_ref() {
            Some(bus) => bus.send_inbox(child_id, BusMessage::Inbox { text }),
            None => Err("no bus wired".into()),
        }
    }

    /// Abort the viewed child's current turn via its live runner handle. The
    /// child's turn-cancel token is set per turn by the drive loop and
    /// cleared when the turn ends, so this only cancels a running model
    /// call; the run continues (next turn, fresh token). A stale Weak (the
    /// child completed and dropped) is pruned so the registry does not leak
    /// across a long-lived parent.
    fn cancel_child_turn(&self, child_id: &str) -> bool {
        let mut registry = match self.children.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some(entry) = registry.get(child_id) else {
            return false;
        };
        match entry.runner.upgrade() {
            Some(runner) => {
                runner.cancel_turn();
                true
            }
            None => {
                registry.remove(child_id);
                false
            }
        }
    }

    /// Kill a background child: cancel its lifecycle token (abort, terminal)
    /// via the live runner handle, distinct from cancel_child_turn (per-turn,
    /// non-terminal). A stale Weak (the child completed and dropped) is pruned.
    /// The detached driver's finalize_child records the SubagentReturn
    /// boundary (status interrupted) + the concurrency permit releases when
    /// the driver task ends.
    fn kill_child(&self, child_id: &str) -> bool {
        let mut registry = match self.children.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some(entry) = registry.get(child_id) else {
            return false;
        };
        match entry.runner.upgrade() {
            Some(runner) => {
                runner.abort();
                true
            }
            None => {
                registry.remove(child_id);
                false
            }
        }
    }

    fn kill_all_children(&self) -> usize {
        let registry = match self.children.lock() {
            Ok(g) => g,
            Err(_) => return 0,
        };
        let mut killed = 0;
        for entry in registry.values() {
            if let Some(runner) = entry.runner.upgrade() {
                runner.abort();
                killed += 1;
            }
        }
        killed
    }

    fn join_children(&self) -> PFut<'_, ()> {
        let children = Arc::clone(&self.children);
        let drivers = Arc::clone(&self.child_drivers);
        let worktree_controller = self.worktree_controller.clone();
        Box::pin(async move {
            // Cancel the children's lifecycle tokens and the drivers' close
            // tokens, then close the gate and drain, all under the two locks.
            // The guards must not live across the awaits below: a mutex guard
            // is not Send, so the inner scope drops both before the first
            // yield. The lock order (drivers then children) and the absence of
            // an await under either lock keep a registration racing the
            // shutdown seeing either a cancelled token or a closed gate, never
            // neither. A poisoned lock still holds a consistent map (the
            // entries are independently owned, and the writes under it are
            // single infallible map operations), so it is recovered and fully
            // enumerated rather than read as an empty close.
            let live: Vec<Arc<Runner>> = {
                let mut gate = drivers.lock().unwrap_or_else(PoisonError::into_inner);
                let kids = children.lock().unwrap_or_else(PoisonError::into_inner);
                for entry in kids.values() {
                    entry.cancel.cancel();
                }
                for slot in gate.drivers.values() {
                    slot.close.cancel();
                }
                gate.close_and_drain_into();
                kids.values().filter_map(|e| e.runner.upgrade()).collect()
            };
            for runner in &live {
                runner.abort();
            }
            // Take the drained drivers. The gate was closed above inside the
            // lock; the drain ran with it, so these are exactly the drivers
            // registered before the close.
            let pending = drivers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take_drained();
            for (_, slot) in pending {
                join_driver(slot, CHILD_JOIN_TIMEOUT).await;
            }
            // A child force-aborted before its finalize cleanup tail dropped
            // its driver future and skipped cleanup_child, so its per-child
            // worktree is only reachable through the controller's reclaim map.
            // All drivers are joined by this point (no child still running);
            // reclaim every tree left behind so a clean tree is removed before
            // the parent lease releases, rather than leaking its dir + branch.
            if let Some(ctrl) = &worktree_controller {
                ctrl.reclaim_abandoned().await;
            }
        })
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
            children: Arc::clone(&self.children),
            child_drivers: Arc::clone(&self.child_drivers),
            descriptor_store: self.descriptor_store.clone(),
        };
        let trigger = TriggerSource::System {
            hook: hook.to_string(),
        };
        if args.run_in_background {
            Box::pin(spawn_exec::run_background_spawn(
                this, parent_sid, 0, None, trigger, args,
            ))
        } else {
            Box::pin(spawn_exec::run_foreground_spawn(
                this, parent_sid, 0, None, None, trigger, args,
            ))
        }
    }
}

/// Join one drained driver with a bounded budget. The budget is per driver,
/// generous for a real finalize tail, with an abort backstop so a wedged
/// driver cannot hold the shutdown lock forever. Background keeps the full
/// result match so a normal end, a panic, an abort, and a timeout stay four
/// distinct outcomes and the panic is announced; foreground confirms its done
/// signal closed rather than treating the abort as the stop; an un-attached
/// slot waits on done alone (its driver ends when its task ends).
async fn join_driver(slot: DriverSlot, budget: Duration) {
    match slot.entry {
        Some(DriverEntry::Background(mut handle)) => {
            let stopped = match timeout(budget, &mut handle).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    handle.abort();
                    handle.await
                }
            };
            if let Err(err) = stopped
                && err.is_panic()
            {
                tracing::warn!("child driver panicked during shutdown: {err}");
            }
        }
        Some(DriverEntry::Foreground { abort }) => {
            let mut done = slot.done;
            if timeout(budget, &mut done).await.is_err() {
                abort.abort();
                let _ = done.await;
            }
        }
        None => {
            let _ = slot.done.await;
        }
    }
}

/// Announce a child before its first status event can be published.
pub(super) fn announce_spawn(bus: Option<&Arc<AgentBus>>, child: ChildDescriptor) {
    if let Some(bus) = bus {
        bus.publish(spawned_topic(), BusMessage::Spawned { child });
    }
}

/// Close a child's inbox after its run so a parent's send_inbox errors
/// rather than queueing into a dead receiver. Mirrors announce_spawn.
fn close_child_inbox(bus: Option<&Arc<AgentBus>>, child_id: &str) {
    if let Some(bus) = bus {
        bus.unregister_inbox(child_id);
    }
}

/// Fire SubagentStart at the durable spawn boundary. No-op when no hook-fire
/// seam is wired (no registry). The signal lands in the parent log so replay
/// pairs it with the SubagentSpawn / SubagentReturn boundaries.
pub(super) async fn fire_subagent_start(
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

/// Stamp SpawnedBy provenance on a child session's descriptor. Called at the
/// spawn boundary, before the child runs, so a live child is never read as one
/// of the user's own sessions. The delegation record the child opens with is
/// its first durable append, which materializes the descriptor on a store that
/// materializes one, so the update normally edits a file already there.
/// Best-effort: a stamp failure logs and continues (the child still ran).
pub(super) fn stamp_spawned_by(
    descriptor_store: &Option<Arc<dyn SessionDescriptorStore>>,
    child_sid: SessionId,
    parent_sid: SessionId,
    subagent_type: &str,
    task_id: &str,
) {
    if let Some(store) = descriptor_store {
        let parent_str = parent_sid.to_string();
        let prov = SessionProvenance::SpawnedBy {
            parent_session_id: parent_str,
            subagent_type: subagent_type.to_string(),
            task_id: task_id.to_string(),
        };
        let outcome = store.update_descriptor(child_sid, &mut |descriptor| {
            descriptor.provenance = prov.clone();
        });
        // Written means there was a descriptor to edit; anything else -- a
        // store that materializes none on a first append, or an update that
        // failed -- leaves the provenance unrecorded, so create a descriptor
        // here, in the same shape the repair pass writes for a child whose
        // boundary write was lost.
        if !matches!(outcome, Ok(DescriptorUpdate::Written)) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let descriptor = SessionDescriptor::delegated_child(prov, now);
            if let Err(e) = store.write_descriptor(child_sid, &descriptor) {
                tracing::warn!("child provenance write failed: {e}");
            }
        }
    }
}

/// Map a terminal RunResult to a status label + the summary text the parent
/// sees. FinalOutput carries the answer; other terminals fall back to the
/// last assistant text in the child log (the partial result) so a max-turns
/// or interrupted child is not silently empty.
///
/// The child's usage is not read here: the run result carries only its direct
/// provider calls, so the parent records the subtree total instead, read from
/// durable state after the run. See child_subtree_usage.
fn terminal_summary(
    r: RunResult,
    child_log: &[houyicoder_context::SessionLogEntry],
) -> (String, String) {
    match r.outcome {
        RunOutcome::FinalOutput(t) => ("completed".to_string(), t),
        RunOutcome::MaxTurnsReached { .. } => ("max_turns".to_string(), partial_of(child_log)),
        RunOutcome::Interrupted(_) | RunOutcome::Interruption(_) => {
            ("interrupted".to_string(), partial_of(child_log))
        }
        RunOutcome::VerifyFailed(_) => ("verify_failed".to_string(), partial_of(child_log)),
        RunOutcome::Handoff(_) => ("handoff".to_string(), partial_of(child_log)),
    }
}

/// The last non-empty assistant text in the child log: the partial result a
/// non-final terminal leaves behind.
fn partial_of(child_log: &[houyicoder_context::SessionLogEntry]) -> String {
    extract_last_assistant(child_log).unwrap_or_default()
}

/// Walk the child log backwards for the last assistant message carrying text;
/// the partial result a non-final terminal leaves behind. None when no
/// assistant message (or none with text) was emitted.
fn extract_last_assistant(events: &[houyicoder_context::SessionLogEntry]) -> Option<String> {
    for ev in events.iter().rev() {
        if let SessionEvent::AssistantMessage { ref text, .. } = ev.event
            && !text.is_empty()
        {
            return Some(text.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests;
