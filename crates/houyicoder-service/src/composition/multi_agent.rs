//! The multi-agent runtime: the SpawnHandle impl the agent tool calls.
//! Holds the parent's components (registry, store, provider, tools, config,
//! worktree controller, cwd) and on a foreground spawn drives the child runner to
//! a terminal state, records the SubagentReturn boundary, and returns the
//! result the tool maps into its tool_result.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use houyicoder_api::hook_fire::HookFire;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_api::skill::SkillRegistry;
use houyicoder_api::spawn::{SpawnArgs, SpawnFailure, SpawnHandle, SpawnOutcome};
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::CancellationToken;
use houyicoder_async::PFut;
use houyicoder_async::bus::MessageBus;
use houyicoder_context::{
    DescriptorUpdate, HookEventKind, HookFirePayload, PromptCoverage, SessionDescriptor,
    SessionDescriptorStore, SessionId, SessionProvenance,
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
use houyicoder_core::agent::multi_agent::spawn::{ChildHandle, RunnerInheritance, TriggerSource};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::worktree_controller::WorktreeController;
use houyicoder_core::agent::{ConditionalSkillActivator, HookRegistry};
use houyicoder_core::agent::{RunOutcome, RunResult, Runner, ToolRegistry};
use std::env::current_dir;
use std::mem::take;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::Notify;
use tokio::sync::oneshot::{Receiver, Sender, channel};
use tokio::task::AbortHandle;
use tokio::task::JoinHandle;
use tokio::time::timeout;

mod children;
mod drive;
mod orphan_reconcile;
use children::{
    AdmissionLease, ChildAdmission, ChildEntry, ChildReservation, ChildWatch, RunningChildren,
    join_child,
};
mod spawn_exec;
mod strip;
pub(crate) use strip::last_assistant_text;

/// Per-child budget the shutdown join gives a child's finalize tail before
/// aborting it. Generous for real cleanup work (worktree removal, boundary
/// writes) so a healthy tail finishes inside it; the abort backstop keeps a
/// wedged task from holding the lock forever. Under test the budget
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
    /// The parent's skill registry, shared with every child: a child holds the
    /// Skill tool, and without it the tool has no listing or resolver behind
    /// it. The activator travels with it, since it is what hides a
    /// paths-gated skill until those paths are touched.
    pub skill_registry: Option<Arc<dyn SkillRegistry>>,
    pub conditional: Option<Arc<dyn ConditionalSkillActivator>>,
    /// The parent's hook registry, so a child's own tool calls meet the same
    /// write-time gates instead of becoming a path around them.
    pub hooks: Option<Arc<HookRegistry>>,
}

#[derive(Clone)]
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
    /// without holding the Arc (the child's task moves the Arc into itself;
    /// a Weak upgrades only while the child is still running). Stale entries
    /// (child completed + dropped) are pruned on a failed upgrade.
    children: Arc<Mutex<HashMap<String, ChildEntry>>>,
    /// The children still running, plus the gate that refuses new ones once a
    /// shutdown starts. The gate and the registry share one lock, so closing
    /// the gate and setting the running children aside is atomic against a
    /// child being admitted: the admission either lands before the close (the
    /// child is waited for) or is refused (the caller waits on it inline),
    /// never silently after.
    running: Arc<Mutex<RunningChildren>>,
    /// Descriptor store for stamping SpawnedBy provenance on child sessions.
    descriptor_store: Option<Arc<dyn SessionDescriptorStore>>,
    /// Skill registry + activator + hooks inherited by every child, so a
    /// sub-agent discovers and loads skills natively and answers to the same
    /// write-time gates its parent does.
    skill_registry: Option<Arc<dyn SkillRegistry>>,
    conditional: Option<Arc<dyn ConditionalSkillActivator>>,
    hooks: Option<Arc<HookRegistry>>,
    /// The parent's compaction components, installed on every child runner.
    inherited: RunnerInheritance,
    /// The foreground delegation wait budget, in milliseconds. Zero leaves a
    /// foreground child blocking; a non-zero value sends it to the background
    /// once it elapses.
    delegation_wait_ms: u64,
    /// The child sessions a spawn is continuing right now, claimed before
    /// construction so two resumes cannot drive one session at once.
    reserved: Arc<Mutex<HashSet<String>>>,
}

/// One live child: its runner (Weak, so a finished child drops the entry's
/// reach) and its lifecycle cancel token. The token is level-triggered and
/// exists from the spawn boundary, before the run's first poll mints the
/// turn token — so a shutdown stop issued while the child's task has not
/// started the run still reaches the child instead of firing at a runner with
/// no token to cancel.
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
            running: Arc::new(Mutex::new(RunningChildren::default())),
            descriptor_store: deps.descriptor_store,
            skill_registry: deps.skill_registry,
            conditional: deps.conditional,
            hooks: deps.hooks,
            inherited: RunnerInheritance::default(),
            delegation_wait_ms: 0,
            reserved: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Install the parent's compaction components on every child this runtime
    /// spawns. The composition root calls this once, before the runtime is
    /// shared, because the components are fixed for the session.
    pub(super) fn with_inheritance(mut self, inherited: RunnerInheritance) -> Self {
        self.inherited = inherited;
        self
    }

    /// Claim a session id a spawn is about to continue.
    ///
    /// None while the id is running or already claimed: a resume that cannot
    /// claim its target must not write into a session another run owns. The
    /// claim is released by the guard, so the caller must keep it until the
    /// child's task ends.
    pub(super) fn reserve_child(&self, child_id: &str) -> Option<ChildReservation> {
        let running = self
            .children
            .lock()
            .map(|mut registry| {
                registry.retain(|_, entry| entry.runner.upgrade().is_some());
                registry.contains_key(child_id)
            })
            .unwrap_or(true);
        if running {
            return None;
        }
        let Ok(mut reserved) = self.reserved.lock() else {
            return None;
        };
        if !reserved.insert(child_id.to_string()) {
            return None;
        }
        Some(ChildReservation::new(
            child_id.to_string(),
            Arc::clone(&self.reserved),
        ))
    }

    /// Register a live child runner so a later per-turn abort can reach it.
    /// The entry's runner Weak drops to nothing when the child's Arc is
    /// released (the child's task ends); cancel_child_turn prunes the entry
    /// it queries. To keep a long-lived parent's registry from accumulating
    /// stale entries for completed children that were never Esc'd, sweep
    /// dead entries on each register (amortized: one sweep per spawn, not
    /// per poll).
    pub(super) fn register_child(
        &self,
        child_id: &str,
        runner: &Arc<Runner>,
        cancel: CancellationToken,
        backgrounded: Option<Arc<AtomicBool>>,
        handover: Option<Arc<Notify>>,
    ) {
        if let Ok(mut g) = self.children.lock() {
            g.retain(|_, entry| entry.runner.upgrade().is_some());
            g.insert(
                child_id.to_string(),
                ChildEntry {
                    runner: Arc::downgrade(runner),
                    cancel,
                    backgrounded,
                    handover,
                },
            );
        }
    }

    /// Admit a child before its first side effect.
    ///
    /// None once shutdown has closed the gate: the spawn is refused instead of
    /// creating a child the join would miss. A poisoned lock fails the
    /// admission, which the caller reads as a refusal before any child exists.
    fn admit_child(&self, close: CancellationToken) -> Option<ChildAdmission> {
        self.running.lock().ok().and_then(|mut g| g.admit(close))
    }

    /// Whether shutdown has closed the gate. A cheap read the foreground path
    /// takes before queueing for a concurrency slot; a poisoned lock reads as
    /// true (fail closed), and the later admission still refuses atomically.
    fn is_shutting_down(&self) -> bool {
        self.running.lock().map(|g| g.closing).unwrap_or(true)
    }

    /// Replace the concurrency gate. Production uses the default gate from
    /// new(); tests inject a small-cap gate to exercise saturation without
    /// spinning up many real children.
    pub(super) fn with_gate(mut self, gate: Arc<ConcurrencyGate>) -> Self {
        self.gate = gate;
        self
    }

    /// Override the foreground delegation wait budget. Tests drive a small
    /// budget to exercise the auto-background path in milliseconds.
    pub(super) fn with_delegation_wait_ms(mut self, ms: u64) -> Self {
        self.delegation_wait_ms = ms;
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

/// Build the child-spawn port attached to the parent runner, carrying the
/// parent's compaction components into every child.
pub(super) fn build_runtime(
    deps: MultiAgentDeps,
    inherited: RunnerInheritance,
) -> Arc<dyn SpawnHandle> {
    Arc::new(MultiAgentRuntime::new(deps).with_inheritance(inherited))
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
        let this = self.clone();
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
    /// The detached child's finalize_child records the SubagentReturn
    /// boundary (status interrupted), and the concurrency permit releases when
    /// the task ends.
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

    /// Hand a running foreground child to the background: flip the mode its
    /// ending is published with and release the spawn call waiting on it. The
    /// child keeps its runner, slot, and task, so nothing is restarted. A
    /// child already in the background reports false.
    fn background_child(&self, child_id: &str) -> bool {
        let registry = match self.children.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some(entry) = registry.get(child_id) else {
            return false;
        };
        let (Some(flag), Some(handover)) = (&entry.backgrounded, &entry.handover) else {
            return false;
        };
        // Already in the background: nothing is waiting and the ending
        // already notifies.
        if flag.swap(true, Ordering::AcqRel) {
            return false;
        }
        handover.notify_one();
        true
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
        let running = Arc::clone(&self.running);
        let worktree_controller = self.worktree_controller.clone();
        Box::pin(async move {
            // End the children's lifecycle tokens and the running children's
            // close tokens, then close the gate and set them aside, all under
            // the two locks. The guards must not live across the awaits below:
            // a mutex guard is not Send, so the inner scope drops both before
            // the first yield. The lock order (running then children) and the
            // absence of an await under either lock keep an admission racing
            // the shutdown seeing either a cancelled token or a closed gate,
            // never neither. A poisoned lock still holds a consistent map (the
            // entries are independently owned, and the writes under it are
            // single infallible map operations), so it is recovered and fully
            // enumerated rather than read as an empty close.
            let live: Vec<Arc<Runner>> = {
                let mut gate = running.lock().unwrap_or_else(PoisonError::into_inner);
                let kids = children.lock().unwrap_or_else(PoisonError::into_inner);
                for entry in kids.values() {
                    entry.cancel.cancel();
                }
                for child in gate.children.values() {
                    child.close.cancel();
                }
                gate.close_and_drain();
                kids.values().filter_map(|e| e.runner.upgrade()).collect()
            };
            for runner in &live {
                runner.abort();
            }
            // Take the children set aside above. The gate closed inside the
            // lock, so these are exactly the children admitted before the
            // close.
            let pending = running
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take_drained();
            for (_, child) in pending {
                join_child(child, CHILD_JOIN_TIMEOUT).await;
            }
            // A child force-aborted before its finalize cleanup tail dropped
            // its task and skipped cleanup_child, so its per-child worktree is
            // only reachable through the controller's reclaim map. Every child
            // is joined by this point, so reclaim every tree left behind: a
            // clean tree is removed before the parent lease releases, rather
            // than leaking its dir and branch.
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
        let this = self.clone();
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

    fn reconcile_orphans(&self, parent_sid: SessionId) -> PFut<'_, ()> {
        let store = Arc::clone(&self.store);
        let children = Arc::clone(&self.children);
        Box::pin(async move {
            orphan_reconcile::run(&store, &children, parent_sid).await;
        })
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
pub(super) fn record_provenance(
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
        // Written means there was a descriptor to edit, and Unchanged means the
        // one there already states this provenance. Anything else -- a store
        // that materializes none on a first append, or an update that failed
        // -- leaves the provenance unrecorded, so create a descriptor here.
        // Complete is truthful at this point: the child has written nothing
        // yet and its runner observes every later append.
        if !matches!(
            outcome,
            Ok(DescriptorUpdate::Written | DescriptorUpdate::Unchanged)
        ) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let descriptor =
                SessionDescriptor::delegated_child(prov, now, PromptCoverage::Complete);
            if let Err(e) = store.write_descriptor(child_sid, &descriptor) {
                tracing::warn!("child provenance write failed: {e}");
            }
        }
    }
}

/// Map a terminal RunResult to a status label + the summary text the parent
/// sees. FinalOutput carries the answer; other terminals fall back to the
/// last assistant text the child's newest rows hold (the partial result) so a
/// max-turns or interrupted child is not silently empty.
///
/// The child's usage is not read here: the run result carries only its direct
/// provider calls, so the parent records the subtree total instead, read from
/// durable state after the run. See child_subtree_usage.
fn terminal_summary(
    r: RunResult,
    child_tail: &[houyicoder_context::SessionLogEntry],
) -> (String, String) {
    match r.outcome {
        RunOutcome::FinalOutput(t) => ("completed".to_string(), t),
        RunOutcome::MaxTurnsReached { .. } | RunOutcome::BudgetExceeded { .. } => {
            ("max_turns".to_string(), partial_summary(child_tail))
        }
        RunOutcome::Interrupted(_) | RunOutcome::Interruption(_) => {
            ("interrupted".to_string(), partial_summary(child_tail))
        }
        RunOutcome::VerifyFailed(_) => ("verify_failed".to_string(), partial_summary(child_tail)),
        RunOutcome::Handoff(_) => ("handoff".to_string(), partial_summary(child_tail)),
    }
}

/// The last non-empty assistant text the child's newest rows hold: the partial
/// result a non-final terminal leaves behind.
fn partial_summary(child_tail: &[houyicoder_context::SessionLogEntry]) -> String {
    last_assistant_text(child_tail).unwrap_or_default()
}

#[cfg(test)]
mod tests;
