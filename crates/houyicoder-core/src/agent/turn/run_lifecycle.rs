//! Public run entry points and per-run state initialization.

use houyicoder_context::{SessionEvent, SessionId, SessionLogEntry};
use houyicoder_protocol::llm::Usage;
use tokio_util::sync::CancellationToken;

use crate::agent::append::new_event;
use crate::agent::cache_liveness::CachedPrefixState;
use crate::agent::context::cas_views::CasViewState;
use crate::agent::fact;
use crate::agent::input_queue::InputQueue;
use crate::agent::memory::MemoryRuntime;
use crate::agent::redundancy::RedundancyTracker;
use crate::agent::runner_config::{
    DEFAULT_SNAPSHOT_SIZE_CAP_BYTES, DEFAULT_SNAPSHOT_TTL_SECS, RunnerConfig,
};
use crate::agent::skill_slash::SkillSlashOutcome;
use crate::agent::turn::result::run_completion_status;
use crate::agent::*;
use houyicoder_api::agent_event::{AgentEventHandlers, LivenessPhase, RunLifecycleEvent};
use houyicoder_api::cache_policy::AutoCachePolicy;
use houyicoder_api::cost_model::AnthropicCostModel;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::session::SessionLog;
use houyicoder_api::skill::SkillRegistry;
use houyicoder_api::skill::grant::SkillGrantStore;
use houyicoder_api::skill::grant::resolve_entitlements;
use houyicoder_api::spawn::AgentIdentity;
use std::collections::{HashSet, VecDeque};
#[cfg(test)]
use std::env::temp_dir;
use std::env::var;
#[cfg(test)]
use std::process::id;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

impl Runner {
    /// Construct a runner that shares an already-Arced store. The caller keeps
    /// its own clone so it can replay events (e.g. a TUI rendering the live
    /// transcript) while the runner appends to the same log.
    pub fn new(
        store: Arc<dyn SessionLog>,
        provider: Arc<dyn ModelProvider>,
        tools: ToolRegistry,
        config: RunnerConfig,
    ) -> Self {
        let observability = obs_wire::new_log(provider.capabilities().context_window);
        // Seed Explicit for the resolved id by default; the composition root
        // calls with_model_choice(Default) when it resolved the sentinel. The
        // apply path overwrites this with each pick's real intent.
        let initial_model = config.model.clone();
        let memory = MemoryRuntime::new(store.clone());
        let runner = Self {
            store,
            provider,
            tools,
            config,
            inference: Arc::new(RwLock::new(InferenceConfig {
                choice: ModelChoice::Explicit {
                    id: initial_model.clone(),
                },
                model: initial_model,
                effort: None,
                speed: SpeedMode::Standard,
            })),
            catalog_resolver: None,
            context_builder: ContextBuilder::new(),
            events: AgentEventHandlers::default(),
            inbox: Mutex::new(None),
            startup_warnings: Mutex::new(Vec::new()),
            breaker: None,
            usage: Arc::new(Mutex::new(UsageAccumulator::default())),
            run_budget: None,
            observability,
            cancel: Mutex::new(None),
            aborted: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            drive_active: AtomicBool::new(false),
            turn_cancel: Mutex::new(None),
            user_turn: Mutex::new(UserTurn::default()),
            verify_gate: None,
            undo_stack: None,
            snapshot_store: None,
            snapshot_ttl_secs: DEFAULT_SNAPSHOT_TTL_SECS,
            snapshot_size_cap_bytes: DEFAULT_SNAPSHOT_SIZE_CAP_BYTES,
            summarizer: Arc::new(manifest::HeuristicSummarizer),
            memory,
            prompt_recorder: None,
            skill_registry: None,
            sandbox_session: None,
            worktree_controller: None,
            skill_grants: None,
            active_skill: Arc::new(Mutex::new(None)),
            hooks: None,
            registrar: None,
            conditional: None,
            skill_reloader: None,
            cache_policy: Arc::new(AutoCachePolicy),
            cost_model: Arc::new(AnthropicCostModel),
            recall_meter: Arc::new(AtomicU32::new(0)),
            workspace_probe: None,
            compaction_suppression: AtomicU8::new(0),
            compaction_transient_failures: AtomicU32::new(0),
            cache_prev_read: Mutex::new(None),
            cache_compact_flag: AtomicBool::new(false),
            cache_model_switch_flag: AtomicBool::new(false),
            cache_prev_prefix: Mutex::new(None),
            last_request_model: Mutex::new(None),
            cached_prefix: Arc::new(CachedPrefixState::new()),
            cas_views: Arc::new(CasViewState::new("tokens")),
            result_catalog: None,
            reducer: None,
            input_queue: InputQueue::new(),
            queued_notifications: Mutex::new(VecDeque::new()),
            redundancy: Mutex::new(RedundancyTracker::new()),
            denied_agents: Arc::new(HashSet::new()),
            spawn_handle: None,
            agent_identity: AgentIdentity::top_level(),
        };
        runner.wire_cache_liveness_policy();
        runner
    }

    /// Advance the current user turn's model-call counter and return it. The
    /// drive loop caps on it so max_turns bounds one user turn's tool loop,
    /// not the session's accumulated turns.
    pub(crate) fn bump_user_turn(&self) -> u32 {
        self.user_turn.lock().expect("user_turn lock").spend_call()
    }

    /// Read the current user turn's model-call count.
    pub(crate) fn user_turn(&self) -> u32 {
        self.user_turn.lock().expect("user_turn lock").calls()
    }

    /// Start a fresh user turn: a new max_turns budget and no work measured.
    pub(crate) fn reset_user_turn(&self) {
        self.user_turn.lock().expect("user_turn lock").begin();
    }

    /// Send a liveness heartbeat in the given phase. A delegated child runs
    /// under a watchdog that cuts it after a stretch with no progress. The
    /// heartbeat is the only signal that distinguishes slow work from a
    /// stalled run, so the streaming path calls this as events arrive, the
    /// tool path calls it on each output tick, and the approval path holds
    /// off the idle cut while a card is on screen.
    pub fn heartbeat(&self, phase: LivenessPhase) {
        self.events
            .emit_run_lifecycle(RunLifecycleEvent::Heartbeat { phase });
    }

    /// Run the agent on a user input. Appends the user event, then drives the
    /// loop: RunAgain → prepare + complete + append + resolve; FinalOutput →
    /// return; Handoff → return; Interruption → return (caller resumes).
    pub async fn run(&self, session: SessionId, user_input: String) -> Result<RunResult, RunError> {
        let Some(_drive) = self.begin_drive() else {
            return Err(RunError::Busy);
        };
        self.prune_snapshots();
        self.reset_run_state();
        self.reset_user_turn();
        self.reapply_skill_entitlements();
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex") = Some(token.clone());
        let pending_facts = fact::extract_save_facts(&user_input);
        if let Some(handle) = self.spawn_handle() {
            handle.reconcile_orphans(session).await;
        }
        // Before the user input lands: an orphan result adjacent to its call.
        self.reconcile_tool_results(session).await?;
        let skill_meta = self.resolve_skill_slash(session, &user_input).await;
        self.append_user_input(session, user_input).await?;
        match skill_meta {
            SkillSlashOutcome::NotASkill => {}
            SkillSlashOutcome::Prepared {
                name,
                body,
                untrusted,
            } => {
                let appended = self
                    .store
                    .append(new_event(
                        session,
                        SessionEvent::SkillBody {
                            skill_name: name,
                            content: body,
                            agent_id: None,
                            untrusted,
                        },
                    ))
                    .await;
                if let Err(e) = appended {
                    let failed: Result<RunResult, RunError> = Err(e.into());
                    self.settle_turn(session, None, &failed).await;
                    return failed;
                }
            }
            SkillSlashOutcome::Refused(notice) => {
                self.emit_system_line(notice);
                let result = Ok(RunResult {
                    outcome: RunOutcome::FinalOutput(String::new()),
                    turns: 0,
                    usage: Usage::default(),
                });
                self.settle_turn(session, None, &result).await;
                return result;
            }
        }
        if let Err(e) = self.memory.recall(session).await {
            let failed: Result<RunResult, RunError> = Err(e);
            self.settle_turn(session, None, &failed).await;
            return failed;
        }
        if let Err(e) = self.inject_skill_listing_and_body(session).await {
            let failed: Result<RunResult, RunError> = Err(e);
            self.settle_turn(session, None, &failed).await;
            return failed;
        }
        let started = Instant::now();
        let result = self.drive_loop(session, 0, Usage::identity(), &token).await;
        self.settle_turn(session, Some(started), &result).await;
        self.emit_run_result(&result);
        if let Ok(_) = result
            && let Some(memory) = self.memory.provider()
        {
            for entry in pending_facts {
                if let Err(e) = memory.add(entry) {
                    tracing::warn!("memory write failed: {e}");
                }
            }
        }
        result
    }

    /// Run on a session pre-seeded with a cloned event prefix plus a user
    /// input. Used by the forked extraction runner. The caller guarantees
    /// the prefix ends at a stop boundary (no orphan ToolCall).
    pub async fn run_forked(
        &self,
        session: SessionId,
        prefix: &[SessionLogEntry],
        user_input: String,
    ) -> Result<RunResult, RunError> {
        let Some(_drive) = self.begin_drive() else {
            return Err(RunError::Busy);
        };
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex") = Some(token.clone());
        for ev in prefix {
            self.store
                .append(new_event(session, ev.event.clone()))
                .await?;
        }
        self.append_user_input(session, user_input).await?;
        // A forked run is a fresh turn: reset the max_turns budget.
        self.reset_user_turn();
        let started = Instant::now();
        let result = self.drive_loop(session, 0, Usage::identity(), &token).await;
        self.settle_turn(session, Some(started), &result).await;
        result
    }

    /// Consume queued completion notifications as a turn. No user input is
    /// written; the notifications are the turn's material. Returns an empty
    /// result when nothing is queued, so the wake signal cannot provoke a
    /// paid no-op turn.
    pub async fn run_notification_turn(&self, session: SessionId) -> Result<RunResult, RunError> {
        let Some(_drive) = self.begin_drive() else {
            return Err(RunError::Busy);
        };
        if self.queued_notification_count() == 0 {
            return Ok(RunResult {
                outcome: RunOutcome::FinalOutput(String::new()),
                turns: 0,
                usage: Usage::default(),
            });
        }
        self.prune_snapshots();
        self.reset_run_state();
        self.reset_user_turn();
        self.reapply_skill_entitlements();
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex") = Some(token.clone());
        self.drain_turn_boundary(session).await?;
        let started = Instant::now();
        let result = self.drive_loop(session, 0, Usage::identity(), &token).await;
        self.settle_turn(session, Some(started), &result).await;
        self.emit_run_result(&result);
        result
    }

    /// Continue a run paused on Interruption. Applies caller decisions to
    /// pending approvals: approved calls execute, rejected calls get a
    /// rejection-note result. Undecided approvals are re-raised as a fresh
    /// Interruption so the caller shows the next dialog. Turn counter is
    /// cumulative from the log; usage restarts at zero.
    pub async fn resume(
        &self,
        session: SessionId,
        decisions: &[ApprovalDecision],
    ) -> Result<RunResult, RunError> {
        let Some(_drive) = self.begin_drive() else {
            return Err(RunError::Busy);
        };
        // A resume is also an entry: the scan runs once per runtime.
        if let Some(handle) = self.spawn_handle() {
            handle.reconcile_orphans(session).await;
        }
        let aborted = match self.aborted_short_circuit(session).await {
            Ok(aborted) => aborted,
            Err(e) => {
                let failed: Result<RunResult, RunError> = Err(e);
                self.settle_turn(session, None, &failed).await;
                return failed;
            }
        };
        if let Some(r) = aborted {
            let result = Ok(r);
            self.settle_turn(session, None, &result).await;
            self.emit_run_result(&result);
            self.finalize_input_buffer(&result);
            return result;
        }
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex") = Some(token.clone());
        let started = Instant::now();
        let remaining = match self.apply_decisions(session, decisions, &token).await {
            Ok(remaining) => remaining,
            Err(e) => {
                let failed: Result<RunResult, RunError> = Err(e);
                self.settle_turn(session, Some(started), &failed).await;
                return failed;
            }
        };
        if !remaining.is_empty() {
            self.mark_paused();
            let result = Ok(RunResult {
                outcome: RunOutcome::Interruption(remaining),
                turns: self.user_turn(),
                usage: Usage::default(),
            });
            self.settle_turn(session, Some(started), &result).await;
            return result;
        }
        let result = self
            .drive_loop(session, self.user_turn(), Usage::identity(), &token)
            .await;
        self.settle_turn(session, Some(started), &result).await;
        self.emit_run_result(&result);
        result
    }

    /// Close a turn by draining the primary recorder and recording the
    /// completion. Every drive leg that returns ends here, and so does every
    /// pause, so a returning leg never leaves a change for a later turn to
    /// surface. A pause records no completion, because the turn it leaves
    /// open is not over.
    pub(crate) async fn settle_turn(
        &self,
        session: SessionId,
        leg: Option<Instant>,
        result: &Result<RunResult, RunError>,
    ) {
        self.memory.drain_primary_changes();
        self.record_run_completion(session, leg, result).await;
    }

    /// Record the end of a turn: how long the turn's drive legs ran, when any
    /// did. Every leg the caller drove is added to the turn's work first, so a
    /// leg that pauses on approvals hands its time to the turn and the leg that
    /// later finishes it reports the whole. A run paused on approvals has not
    /// finished, so the leg that finishes the turn writes the record instead —
    /// the record belongs to the turn's end, whichever call drove it. A turn
    /// that ended without a loop (an abort while paused on an approval) still
    /// records its end: the frontend needs the boundary, and it reports the
    /// work the legs before the pause did. Failure to write is logged, not
    /// fatal: the record feeds the frontend's turn summary, and the run already
    /// has its outcome to report.
    async fn record_run_completion(
        &self,
        session: SessionId,
        leg: Option<Instant>,
        result: &Result<RunResult, RunError>,
    ) {
        let paused =
            matches!(result, Ok(run) if matches!(run.outcome, RunOutcome::Interruption(_)));
        let ms = {
            let mut turn = self.user_turn.lock().expect("user_turn lock");
            if let Some(leg) = leg {
                turn.account(leg.elapsed());
            }
            if paused {
                return;
            }
            turn.take_worked()
                .map(|worked| worked.as_millis().min(u64::MAX as u128) as u64)
        };
        if let Err(e) = self
            .store
            .append(new_event(
                session,
                SessionEvent::RunCompleted {
                    ms,
                    outcome: Some(run_completion_status(result)),
                },
            ))
            .await
        {
            tracing::warn!("run completion record failed: {e}");
        }
    }

    /// Re-apply or clear skill entitlements at the turn boundary. A skill
    /// still active from a prior turn gets its entitlements re-resolved so
    /// a user-approved grant takes effect on the next turn. When no skill
    /// is active, clear the slate.
    pub(crate) fn reapply_skill_entitlements(&self) {
        reapply_skill_entitlements_impl(
            self.active_skill.lock().expect("active_skill lock").clone(),
            self.sandbox_session.as_deref(),
            self.skill_registry.as_deref(),
            self.skill_grants.as_deref(),
        );
    }

    /// Fire background memory once a turn reaches its final output: the
    /// extractor and dream workers run on that outcome alone. The primary
    /// recorder is drained when the turn settles, on every outcome, not here.
    /// Reward capture is withheld when the operator sets HOUYICODER_REWARD_OFF,
    /// which suppresses the dream reward signal only.
    pub(crate) async fn fire_background_memory(&self, session: SessionId) {
        let reward_off = var("HOUYICODER_REWARD_OFF").is_ok();
        let reward = if reward_off {
            None
        } else {
            Some(|| reward_snapshot::capture_reward_snapshot(&self.observability, &self.redundancy))
        };
        self.memory.fire_background(session, reward).await;
    }
}

/// Extracted as a free function so the logic is testable without a Runner.
fn reapply_skill_entitlements_impl(
    active: Option<String>,
    session: Option<&dyn SandboxSession>,
    registry: Option<&dyn SkillRegistry>,
    grants: Option<&SkillGrantStore>,
) {
    if let Some(name) = active
        && let Some(session) = session
        && let Some(registry) = registry
    {
        let desc = registry.find(&name);
        let source = registry.source_for(&name);
        let (mach, allow_launch) = resolve_entitlements(
            grants,
            &name,
            source.as_ref(),
            desc.as_ref()
                .map(|d| d.allowed_mach_services.as_slice())
                .unwrap_or(&[]),
            desc.as_ref().is_some_and(|d| d.allow_app_launch),
        );
        session.clear_skill_grants();
        session.set_extra_mach_services(&mach);
        if allow_launch {
            session.grant_app_launch();
        }
    } else if let Some(session) = session {
        session.clear_skill_grants();
    }
}

#[cfg(test)]
mod tests;
