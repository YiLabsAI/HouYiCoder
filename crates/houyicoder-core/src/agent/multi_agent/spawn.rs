//! Child-agent construction, durable spawn boundaries, and run handles.

use houyicoder_api::agent_event::AgentEventHandlers;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::result_catalog::ResultCatalog;
use houyicoder_api::session::{SessionLog, TrajectoryPosition};
use houyicoder_api::skill::SkillRegistry;
use houyicoder_async::CancellationToken;
use houyicoder_context::{SessionDescriptorStore, SessionEvent, SessionId, SessionProvenance};
use std::sync::{Arc, Mutex};

use crate::agent::append::new_event;
use crate::agent::runner_config::RunnerConfig;
use crate::agent::worktree_controller::{ChildWorktree, WorktreeController};
use crate::agent::{
    ConditionalSkillActivator, HookRegistry, ModelCatalogResolver, Runner, Summarizer,
    ToolOutputReducer, ToolRegistry,
};

use super::bus_types::AgentBus;
use super::bus_types::BusMessage;
use super::bus_types::{ChildDescriptor, ChildRunMode};
use super::child_prompt::resolve_child_effort;
use super::ending::EndingClaim;
use super::registry::IsolationMode;
use super::status_publisher::{ChildLiveness, ChildStatusPublisher};
use houyicoder_api::spawn::AgentIdentity;
use houyicoder_async::bus::MessageBus;
use tokio::sync::mpsc::unbounded_channel;
use tracing::warn;

mod boundary;

pub use boundary::{
    BoundaryOutcome, ChildLedger, OrphanedChild, RecordOwed, completion_notice_pending,
    logged_usage, orphaned_summary, record_orphaned_child, record_subagent_return,
};

/// Cap on spawn nesting. A top-level agent is depth 0; each spawn adds 1.
/// A child cannot delegate further, so the topology is exactly two levels:
/// one parent and its children. A child that could spawn would nest reports
/// inside reports, and every view that summarises a delegation would then
/// have to decide how deep to summarise.
const MAX_SPAWN_DEPTH: u32 = 1;

/// What initiated a spawn: the model via the agent tool, or the system
/// (a hook/gate/automated flow). A system trigger lets a service-layer
/// gate drive the runtime without going through the model. The
/// capability baseline is the parent's session capability at the trigger
/// moment either way, so a system trigger cannot bypass the capability∩
/// rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerSource {
    /// The model delegated via the agent tool (the existing path).
    ModelTool { tool_call_id: String },
    /// A service/hook/gate initiated the spawn (e.g. a review trigger
    /// gate), not the model. The hook string records which flow fired.
    System { hook: String },
}

impl TriggerSource {
    /// The durable form recorded on the SubagentSpawn event so a replay
    /// can tell a model-driven delegation from a system-driven gate.
    pub fn as_durable(&self) -> String {
        match self {
            Self::ModelTool { tool_call_id } => format!("model:{tool_call_id}"),
            Self::System { hook } => format!("system:{hook}"),
        }
    }

    /// Recover the trigger from the durable form a spawn recorded, so a reader
    /// of the log sees the same trigger the writer wrote. A form this codebase
    /// never wrote is None, which a reader treats as an unknown trigger rather
    /// than silently as a model call.
    pub fn from_durable(durable: &str) -> Option<Self> {
        if let Some(tool_call_id) = durable.strip_prefix("model:") {
            return Some(Self::ModelTool {
                tool_call_id: tool_call_id.to_string(),
            });
        }
        durable.strip_prefix("system:").map(|hook| Self::System {
            hook: hook.to_string(),
        })
    }
}

/// Why an outside stop ended a child, recorded by whoever stopped it.
///
/// The ending is published from inside the child's run, before the spawn code
/// that stopped it gets the run back. A stop that wants to say more than
/// "interrupted" therefore has to record its reason where the publisher reads
/// it: the shared slot on the child handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CutReason {
    /// The child's wall-clock ceiling expired. The stage names what it was
    /// doing when it stopped, when that is known.
    TimedOut { secs: u64, stage: Option<String> },
}

/// Reads what a delegated child had produced when its run stopped.
///
/// A child cut short publishes its ending from inside its run, before the
/// spawn code that stopped it regains control, so the publisher cannot be
/// handed the child's text by that code. It reads the child's log through
/// this port instead.
pub trait PartialTranscript: Send + Sync {
    /// The last assistant text the session holds, if it holds any.
    fn last_text(&self, session: SessionId) -> Option<String>;
}

/// The parent's compaction components, shared with every child it spawns.
///
/// A child compresses its own log, so it needs the same compression setup the
/// parent runs with. They travel as one value rather than four loose fields:
/// a runtime either inherited that setup or it did not, and half of it would
/// leave a child compressing one way and reporting another. A child built
/// without this value falls back to the runner defaults, which is what the
/// tests and the stub paths get.
#[derive(Clone, Default)]
pub struct RunnerInheritance {
    /// The summarizer the child's compress stage calls.
    pub summarizer: Option<Arc<dyn Summarizer>>,
    /// The reducer the child's isolate stage applies to large tool results.
    pub reducer: Option<Arc<dyn ToolOutputReducer>>,
    /// The result index the child's search tool reads and its status reports.
    pub result_catalog: Option<Arc<dyn ResultCatalog>>,
    /// The resolver the child reads model limits and defaults from.
    pub catalog_resolver: Option<Arc<dyn ModelCatalogResolver>>,
}

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
    /// What initiated the spawn — the model via the agent tool, or a
    /// system hook/gate. Recorded on the SubagentSpawn boundary so a
    /// replay distinguishes a model delegation from a gate-driven spawn.
    pub trigger: TriggerSource,
    /// Spawn depth of the parent (0 = top-level). The runtime derives this
    /// from the session's spawn ancestry before calling spawn; the child is
    /// depth + 1. Used by the recursion guard to cap nesting.
    pub depth: u32,
    /// The isolation mode the resolved agent definition asked for. Worktree
    /// creates a per-child worktree and restricts the fence; None runs the child
    /// in the parent's tree.
    pub isolation: IsolationMode,
    /// The worktree controller, required when isolation is Worktree. None
    /// with Worktree is a wiring error the runtime must not produce; spawn
    /// fail-closes to WorktreeFenceNarrowFail rather than degrading to no
    /// isolation.
    pub worktree_controller: Option<Arc<WorktreeController>>,
    /// Whether the child blocks the parent or continues independently.
    pub run_mode: ChildRunMode,
    /// The child's lifecycle cancel token, minted by the caller before spawn
    /// and stored on the child handle verbatim. A foreground caller passes a
    /// linked clone of the parent token (a parent abort reaches the child); a
    /// background caller passes a fresh unlinked token. The caller owns the
    /// link decision so the same token also lives in the shutdown admission
    /// credential.
    pub child_cancel: CancellationToken,
    /// The shared in-process bus used for child status and steering.
    pub bus: Option<Arc<AgentBus>>,
    /// The skill registry the parent discovered. The child carries the Skill
    /// tool, so without the registry the tool has no listing and no resolver:
    /// the child cannot see which skills exist, and the model reads a SKILL.md
    /// by absolute path instead. None in tests and the stub path.
    pub skill_registry: Option<Arc<dyn SkillRegistry>>,
    /// The parent's conditional-skill activator, paired with the registry:
    /// it is what hides a paths-gated skill until its paths are touched.
    pub conditional: Option<Arc<dyn ConditionalSkillActivator>>,
    /// The parent's hook registry. A child runs tools, so it answers to the
    /// same write-time gates the parent does; without this a sub-agent is a
    /// path around every project rule.
    pub hooks: Option<Arc<HookRegistry>>,
    /// The parent's compression components. A child runs long enough to
    /// compact, and a child left on the heuristic summarizer would fold its
    /// log into a summary nobody wrote.
    pub inherited: RunnerInheritance,
    /// The parent's descriptor store. The child runner records its own
    /// prompt through it, so a child's descriptor says what that child was
    /// asked to do. None leaves the child with whatever descriptor the
    /// store materializes, which the child never updates.
    pub descriptor_store: Option<Arc<dyn SessionDescriptorStore>>,
    /// How the status publisher reads the child's own text, so an ending cut
    /// short reports what the child produced. None falls back to the ending's
    /// own summary.
    pub partial_transcript: Option<Arc<dyn PartialTranscript>>,
    /// A child session to continue instead of minting a new one.
    ///
    /// The child's turns are already durable in its own log, so resuming
    /// replays that conversation and appends the new prompt to it. Without
    /// this an interrupted delegation has to be paid for again from the
    /// start, which for a long research child is the expensive half.
    pub resume: Option<SessionId>,
    /// A wall-clock ceiling for this child, in seconds. None runs until the
    /// child reaches a terminal of its own.
    pub timeout_secs: Option<u64>,
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
    pub ending: Arc<EndingClaim>,
    pub cut_reason: Arc<Mutex<Option<CutReason>>>,
    pub liveness: Arc<ChildLiveness>,
}

/// Why a spawn was rejected. Budget and capability failures are policy,
/// not panics; the caller surfaces these to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    BudgetExceeded,
    CapabilityDenied,
    SpawnRecursive,
    /// A durable delegation boundary could not be written: the child's own
    /// record, the parent's spawn record, or the parent's return record. The
    /// first two refuse the spawn rather than recording a delegation nothing
    /// can follow to its child; a return record that fails is logged, since
    /// the child has already run to its terminal.
    BoundaryWriteFailed,
    WorktreeFenceNarrowFail,
    /// The session named for continuation does not exist, is not a delegation
    /// of this parent, or is still running. A resume writes into an existing
    /// session, so an unproven target is refused before any side effect.
    InvalidResumeTarget {
        /// What about the target failed, in words the caller can report.
        reason: String,
    },
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

    // A resume continues the named child's own session, so the conversation
    // its log already holds is the one the new prompt lands in. The target is
    // proven before anything below can touch it: a session this parent did not
    // delegate is refused here, not after the child boundary was written.
    let child_sid = match req.resume {
        Some(sid) => {
            validate_resume_target(&req, sid)?;
            sid
        }
        None => SessionId::new(),
    };
    // The child's lifecycle cancel token is supplied by the caller, so the
    // token on the handle is the exact one the caller holds in its shutdown
    // admission credential. Foreground linkage vs background independence is
    // the caller's policy, chosen before spawn, not derived here.
    let cancel = req.child_cancel.clone();

    let (worktree, isolation_str) = enter_child_isolation(&req, child_sid).await?;

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
    let bus = req.bus.clone();
    // The descriptor carries the delegating call's task summary so the fleet
    // row can name what this child is doing; the type alone cannot tell two
    // children of one type apart.
    let child = ChildDescriptor {
        agent_id: child_sid.to_string(),
        agent_type: req.subagent_type.clone(),
        description: req.prompt_summary.clone(),
        run_mode: req.run_mode,
    };

    let mut runner = Runner::new(req.parent_store, req.provider, req.tools, req.config)
        .with_agent_identity(AgentIdentity {
            subagent_type: Some(req.subagent_type.clone()),
            // The child dispatches carry the child's own identity, so a
            // nested agent call reports depth + 1 to the recursion guard.
            depth: req.depth + 1,
            parent_session_id: Some(req.parent_sid.to_string()),
        });
    // The child's prompt enters its own log through the child runner, so the
    // recorder that turns a durable append into a descriptor prompt has to be
    // installed here. Without it the child's descriptor claims whatever
    // coverage the store materialized and never learns what the child was
    // asked, which is the state a resume by id reads.
    if let Some(descriptor_store) = req.descriptor_store.clone() {
        runner = runner.with_descriptor_store(descriptor_store);
    }
    // The child inherits the parent's skill and hook wiring. These are the
    // components the tools it holds read, so a child without them carries the
    // Skill tool with no listing behind it and runs tools outside the
    // project's write-time gates.
    if let Some(registry) = req.skill_registry.clone() {
        runner = runner.with_skill_registry(registry);
    }
    if let Some(conditional) = req.conditional.clone() {
        runner = runner.with_conditional(conditional);
    }
    if let Some(hooks) = req.hooks.clone() {
        runner = runner.with_hooks(hooks);
    }
    runner = inherit_compaction(runner, &req.inherited);
    let mut ending = Arc::new(EndingClaim::new(child.run_mode));
    // Written by whoever stops the child and read by the ending it publishes.
    // It exists on every handle, so a spawn with no bus pays one allocation
    // and the foreground path reports a cut to its caller the same way.
    let cut_reason = Arc::new(Mutex::new(None));
    let liveness = Arc::new(ChildLiveness::new());
    if let Some(bus) = bus {
        let mut events = AgentEventHandlers::default();
        let publisher = ChildStatusPublisher::new(bus.clone(), child)
            .with_cut_report(Arc::clone(&cut_reason), req.partial_transcript.clone())
            .with_liveness(Arc::clone(&liveness));
        // The publisher owns the mode its ending is published with, so the
        // handle carries the publisher's own claim rather than a second copy
        // that could disagree with it.
        ending = publisher.ending();
        events.set_run_lifecycle(Arc::new(publisher));
        runner.set_event_handlers(events);
        // Register a point-to-point inbox so the parent can steer this
        // child mid-task: the drive loop drains the receiver at each turn
        // boundary, appending each Inbox text as a user message. The bus
        // routes send_inbox calls to the registered sender.
        let (tx, rx) = unbounded_channel::<BusMessage>();
        bus.register_inbox(&child_sid.to_string(), tx);
        runner.set_inbox(rx);
    }
    let runner = Arc::new(runner);
    // Children run at the lowest effort tier: a fan-out of sub-agents must
    // not multiply reasoning-token spend. Sticky on the runner so every
    // child request carries it; the active pick outranks any catalog level
    // the child model might default to.
    runner.set_effort(resolve_child_effort());

    // The child's own log opens with the delegation it came from: the parent
    // the boundary names, written first so a failure here leaves no spawn in
    // the parent log, and durable from the child's first record so the
    // lineage does not depend on the descriptor. A resumed session already
    // holds that record, and writing a second one would claim the child was
    // delegated twice.
    if req.resume.is_none() {
        let delegation = new_event(
            child_sid,
            SessionEvent::ChildDelegated {
                parent_session_id: parent_sid.to_string(),
                subagent_type: subagent_type.clone(),
            },
        );
        store_for_boundary
            .append(delegation)
            .await
            .map_err(|_| SpawnError::BoundaryWriteFailed)?;
    }

    let spawn_event = new_event(
        parent_sid,
        SessionEvent::SubagentSpawn {
            child_session_id: child_sid.to_string(),
            subagent_type,
            prompt_summary,
            isolation: isolation_str.to_string(),
            policy: "delegate".to_string(),
            trigger_source: req.trigger.as_durable(),
        },
    );
    store_for_boundary
        .append(spawn_event)
        .await
        .map_err(|_| SpawnError::BoundaryWriteFailed)?;

    Ok(ChildHandle {
        session: child_sid,
        runner,
        cancel,
        worktree,
        ending,
        cut_reason,
        liveness,
    })
}

/// Enter the per-child worktree when the spawn asked for one.
///
/// Fail-closed: a missing controller or a fence setup failure refuses the
/// spawn rather than degrading to no isolation, so no child session is built
/// and no boundary is written. The guard rides on the child handle and the
/// caller drops it on completion to restore the fence. The sync git and
/// sandbox calls block the async caller for the one-shot setup cost.
async fn enter_child_isolation(
    req: &SpawnRequest,
    child_sid: SessionId,
) -> Result<(Option<ChildWorktree>, &'static str), SpawnError> {
    let controller = match req.isolation {
        IsolationMode::None => return Ok((None, "none")),
        IsolationMode::Worktree => req
            .worktree_controller
            .as_ref()
            .ok_or(SpawnError::WorktreeFenceNarrowFail)?,
    };
    let slug = format!("agent-{}", &child_sid.to_string()[..8]);
    // The error this returns carries no reason, so the cause is logged here:
    // without it an operator sees a refused child and no way to tell a busy
    // worktree from a failing git.
    let worktree = controller.enter_for_child(slug).await.map_err(|error| {
        warn!(%error, "child worktree setup failed");
        SpawnError::WorktreeFenceNarrowFail
    })?;
    Ok((Some(worktree), "worktree"))
}

/// Give the child runner the parent's compression components.
///
/// The child compresses its own log, so without these it folds its history
/// with the heuristic summarizer and loses everything a real summary keeps.
/// Each component is installed only when the parent supplied one, which leaves
/// a runtime built without inheritance on the runner defaults.
fn inherit_compaction(mut runner: Runner, inherited: &RunnerInheritance) -> Runner {
    if let Some(summarizer) = inherited.summarizer.clone() {
        runner = runner.with_summarizer(summarizer);
    }
    if let Some(reducer) = inherited.reducer.clone() {
        runner = runner.with_reducer(reducer);
    }
    if let Some(catalog) = inherited.result_catalog.clone() {
        runner = runner.with_result_catalog(catalog);
    }
    if let Some(resolver) = inherited.catalog_resolver.clone() {
        runner = runner.with_catalog_resolver(resolver);
    }
    runner
}

/// How many of the target's first records a resume reads to prove lineage.
///
/// The delegation record is written before a child runs, so it is the target's
/// first record on any session this runtime spawned. A few are read rather
/// than one so a record an older build wrote elsewhere still answers.
const RESUME_PROBE_RECORDS: usize = 16;

/// Prove the named session may be continued by this parent, before the spawn
/// has any side effect.
///
/// A resume writes the new prompt into an existing session, so the target must
/// exist and must be a delegation of this parent. A target that fails either
/// check is refused here: continuing the parent's own session, or a top-level
/// session of the user's, would write a delegated turn into a conversation
/// nobody delegated. Liveness is checked by the runtime that owns the running
/// children, which is the only place that can arbitrate two spawns at once.
fn validate_resume_target(req: &SpawnRequest, child_sid: SessionId) -> Result<(), SpawnError> {
    let parent = req.parent_sid.to_string();
    let descriptor = req
        .descriptor_store
        .as_ref()
        .and_then(|store| store.read_descriptor(child_sid));
    let page = req
        .parent_store
        .trajectory_page(child_sid, TrajectoryPosition::start(), RESUME_PROBE_RECORDS)
        .map_err(|error| {
            warn!(%error, "the resume target's session log could not be read");
            SpawnError::InvalidResumeTarget {
                reason: "its session log could not be read".to_string(),
            }
        })?;
    if descriptor.is_none() && page.events.is_empty() {
        return Err(SpawnError::InvalidResumeTarget {
            reason: "no such session".to_string(),
        });
    }
    // Either durable record proves lineage, and both are checked because each
    // can be missing on its own: the descriptor stamp is best effort, and a
    // store that materializes none leaves only the log record.
    let stamped_by_parent = matches!(
        descriptor.as_ref().map(|descriptor| &descriptor.provenance),
        Some(SessionProvenance::SpawnedBy { parent_session_id, .. })
            if parent_session_id == &parent
    );
    let delegated_by_parent = page.events.iter().any(|entry| {
        matches!(
            &entry.event,
            SessionEvent::ChildDelegated { parent_session_id, .. }
                if parent_session_id == &parent
        )
    });
    if stamped_by_parent || delegated_by_parent {
        return Ok(());
    }
    Err(SpawnError::InvalidResumeTarget {
        reason: "the session is not a delegation of this parent".to_string(),
    })
}

#[cfg(test)]
mod tests;
