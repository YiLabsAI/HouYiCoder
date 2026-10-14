//! Public run entry points and per-run state initialization.

use houyicoder_context::{SessionEvent, SessionId, SessionLogEntry};
use houyicoder_protocol::llm::Usage;
use tokio_util::sync::CancellationToken;

use super::append::new_event;
use super::fact;
use super::runner_config::{
    DEFAULT_SNAPSHOT_SIZE_CAP_BYTES, DEFAULT_SNAPSHOT_TTL_SECS, RunnerConfig,
};
use super::*;
use crate::agent::skill_slash::SkillSlashOutcome;
use cache_liveness::CachedPrefixState;
use houyicoder_api::agent_event::AgentEventHandlers;
use houyicoder_api::cache_policy::AutoCachePolicy;
use houyicoder_api::cost_model::AnthropicCostModel;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::session::SessionLog;
use houyicoder_api::skill::SkillRegistry;
use houyicoder_api::skill::grant::SkillGrantStore;
use houyicoder_api::skill::grant::resolve_entitlements;
use houyicoder_api::spawn::AgentIdentity;
use input_queue::InputQueue;
use memory::MemoryRuntime;
use redundancy::RedundancyTracker;
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
            observability,
            cancel: Mutex::new(None),
            aborted: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            turn_cancel: Mutex::new(None),
            user_turn: Mutex::new(UserTurn::default()),
            verify_gate: None,
            undo_stack: None,
            snapshot_store: None,
            snapshot_ttl_secs: DEFAULT_SNAPSHOT_TTL_SECS,
            snapshot_size_cap_bytes: DEFAULT_SNAPSHOT_SIZE_CAP_BYTES,
            summarizer: Box::new(manifest::HeuristicSummarizer),
            memory,
            skill_registry: None,
            sandbox_session: None,
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
            last_request_model: Mutex::new(None),
            cached_prefix: Arc::new(CachedPrefixState::new()),
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
    pub(super) fn bump_user_turn(&self) -> u32 {
        self.user_turn.lock().expect("user_turn lock").spend_call()
    }

    /// Read the current user turn's model-call count.
    pub(super) fn user_turn(&self) -> u32 {
        self.user_turn.lock().expect("user_turn lock").calls()
    }

    /// Start a fresh user turn: a new max_turns budget and no work measured.
    pub(super) fn reset_user_turn(&self) {
        self.user_turn.lock().expect("user_turn lock").begin();
    }

    /// Run the agent on a user input. Appends the user event, then drives the
    /// loop: RunAgain → prepare + complete + append + resolve; FinalOutput →
    /// return; Handoff → return; Interruption → return (caller resumes).
    pub async fn run(&self, session: SessionId, user_input: String) -> Result<RunResult, RunError> {
        self.prune_snapshots();
        self.reset_run_state();
        // A new user turn starts with a fresh max_turns budget and no carried
        // work: the cap bounds one turn's tool loop, and every exit of this
        // turn settles against state that began here, so a turn abandoned on
        // an approval cannot leak its measured time into the next record.
        self.reset_user_turn();
        self.reapply_skill_entitlements();
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex") = Some(token.clone());
        let pending_facts = fact::extract_save_facts(&user_input);
        // Reconcile orphan ToolCall before appending user input: the
        // interrupted result must land adjacent to its tool_use or the
        // provider rejects with a role-order 400.
        self.reconcile_tool_results(session).await?;
        // Resolve @skill: before appending so the raw text is the UserInput
        // and the body lands as a durable SkillBody (survives compaction).
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
                // The refusal ends the turn even though no leg ran. Without
                // the completion record the frontend folds this turn into
                // the next one; no leg means no measured duration.
                self.settle_turn(session, None, &result).await;
                return result;
            }
        }
        // Both preparation steps run after the user input is persisted, so
        // their failures end an opened turn all the same: settle before
        // returning, or the turn folds into the next one.
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
        let result = self.drive_loop(session, 0, Usage::default(), &token).await;
        self.settle_turn(session, Some(started), &result).await;
        self.emit_run_result(&result);
        // Best-effort fact persistence: failures are logged, not fatal.
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
        let result = self.drive_loop(session, 0, Usage::default(), &token).await;
        self.settle_turn(session, Some(started), &result).await;
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
        let aborted = match self.aborted_short_circuit(session).await {
            Ok(aborted) => aborted,
            Err(e) => {
                // The failed resume ends the turn as a completed one does, so
                // its record lands too: without it the turn stays open and its
                // work carries into the next turn's row. This call drives no
                // leg of its own, so the record reports what the legs before
                // the pause accounted.
                let failed: Result<RunResult, RunError> = Err(e);
                self.settle_turn(session, None, &failed).await;
                return failed;
            }
        };
        if let Some(r) = aborted {
            // Abort skips drive_loop, so finalize here.
            let result = Ok(r);
            // The turn still ends here, so the record closes it: skipping it
            // would leave the aborted turn open and its reasoning and tool
            // calls would fold into the next turn's summary row. This call
            // drives no leg of its own, so the record reports the work the
            // legs before the pause already accounted.
            self.settle_turn(session, None, &result).await;
            self.emit_run_result(&result);
            self.finalize_input_buffer(&result);
            return result;
        }
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex") = Some(token.clone());
        // The leg opens before the decisions are applied: executing the
        // approved tools is work this turn spends, so the clock has to cover it.
        let started = Instant::now();
        let remaining = match self.apply_decisions(session, decisions, &token).await {
            Ok(remaining) => remaining,
            Err(e) => {
                // A released call that could not be recorded leaves the turn
                // unfinished all the same. The record closes it and reports the
                // work this leg did, so a failed resume is not mistaken for a
                // turn still running.
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
            // The turn is still open, so no completion is recorded. The drain
            // runs all the same, so a change a leg landed cannot wait for the
            // leg that finishes the turn. No shipped tool leaves one here, since
            // the only writer of the recorder auto-approves and never pauses.
            self.settle_turn(session, Some(started), &result).await;
            return result;
        }
        // Resume the same user turn from its current per-turn count, so the
        // cap, the reported turns, and the convergence reminder all share the
        // per-turn budget rather than the session's accumulated turns.
        let result = self
            .drive_loop(session, self.user_turn(), Usage::default(), &token)
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
    pub(super) async fn settle_turn(
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
            .append(new_event(session, SessionEvent::RunCompleted { ms }))
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
