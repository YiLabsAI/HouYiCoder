//! Public run entries: run, run_forked, resume. Each is a thin wrapper over
//! drive_loop that sets up per-run state (cancel token, fact extraction,
//! memory recall) at the boundaries a caller hits. Extracted from the main
//! impl so the entry surface and the loop body live apart.

use houyicoder_context::{SessionId, TurnEvent, TurnEventKind};
use houyicoder_protocol::llm::Usage;
use tokio_util::sync::CancellationToken;

use super::append::new_event;
use super::fact;
use super::runner_config::{
    DEFAULT_SNAPSHOT_SIZE_CAP_BYTES, DEFAULT_SNAPSHOT_TTL_SECS, RunnerConfig,
};
use super::*;
use houyicoder_api::provider::ModelProvider;

impl Runner {
    /// Construct a runner that shares an already-Arced store. The caller keeps
    /// its own clone so it can replay events (e.g. a TUI rendering the live
    /// transcript) while the runner appends to the same log.
    pub fn with_shared_store(
        store: Arc<dyn houyicoder_api::session::SessionLog>,
        provider: Arc<dyn ModelProvider>,
        tools: ToolRegistry,
        config: RunnerConfig,
    ) -> Self {
        let observability = obs_wire::new_log(provider.capabilities().context_window);
        let active_model = Arc::new(std::sync::RwLock::new(config.model.clone()));
        let active_effort = Arc::new(std::sync::RwLock::new(None));
        let runner = Self {
            store,
            provider,
            tools,
            config,
            active_model,
            active_effort,
            effort_resolver: None,
            context_builder: ContextBuilder::new(),
            live: None,
            inbox: std::sync::Mutex::new(None),
            startup_warnings: std::sync::Mutex::new(Vec::new()),
            breaker: None,
            usage: Arc::new(std::sync::Mutex::new(UsageAccumulator::default())),
            observability,
            cancel: std::sync::Mutex::new(None),
            aborted: std::sync::atomic::AtomicBool::new(false),
            paused: std::sync::atomic::AtomicBool::new(false),
            verify_gate: None,
            undo_stack: None,
            snapshot_store: None,
            snapshot_ttl_secs: DEFAULT_SNAPSHOT_TTL_SECS,
            snapshot_size_cap_bytes: DEFAULT_SNAPSHOT_SIZE_CAP_BYTES,
            summarizer: Box::new(manifest::HeuristicSummarizer),
            memory: None,
            skill_registry: None,
            hooks: None,
            cache_policy: Arc::new(houyicoder_api::cache_policy::AutoCachePolicy),
            cost_model: Arc::new(houyicoder_api::cost_model::AnthropicCostModel),
            recall_meter: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            workspace_probe: None,
            compact_suppress: std::sync::atomic::AtomicU8::new(0),
            compact_consecutive_failures: std::sync::atomic::AtomicU32::new(0),
            cache_prev_read: std::sync::Mutex::new(None),
            cache_compact_flag: std::sync::atomic::AtomicBool::new(false),
            cache_model_switch_flag: std::sync::atomic::AtomicBool::new(false),
            cached_prefix: std::sync::Arc::new(cache_liveness::CachedPrefixState::new()),
            reducer: None,
            extractor: None,
            dream: None,
            auto_memory: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            auto_dream: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            queued_input: std::sync::Mutex::new(std::collections::VecDeque::new()),
            queued_notifications: std::sync::Mutex::new(std::collections::VecDeque::new()),
            consumed_input: std::sync::Mutex::new(Vec::new()),
            redundancy: std::sync::Mutex::new(redundancy::RedundancyTracker::new()),
            denied_agents: Arc::new(std::collections::HashSet::new()),
            spawn_handle: None,
            agent_identity: houyicoder_api::spawn::AgentIdentity::top_level(),
        };
        runner.wire_cache_liveness_policy();
        runner
    }

    /// Run the agent on a user input. Appends the user event, then drives the
    /// loop: RunAgain → prepare + complete + append + resolve; FinalOutput →
    /// return; Handoff → return; Interruption → return (caller resumes).
    pub async fn run(&self, session: SessionId, user_input: String) -> Result<RunResult, RunError> {
        // Prune expired snapshots at the start of each run — a natural trigger
        // (a new run is starting, clean up old snapshots that exceed TTL/size cap).
        // Skips snapshots still referenced by the undo stack.
        self.prune_snapshots();
        self.reset_run_state();
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex") = Some(token.clone());
        // Deterministic fact extraction: scan the user input for explicit
        // save signals before appending. Extracted facts are written
        // atomically after the run completes so the store reflects them
        // for the next session. No model classifier on the hot path — only
        // structured patterns the user types deliberately.
        let pending_facts = fact::extract_save_facts(&user_input);
        // Reconcile orphan ToolCall from a prior hard crash / disconnect before
        // appending this turn's user input. The interrupted result must land
        // adjacent to its tool_use: build_request_body emits role:"tool", which
        // must immediately follow the assistant turn that issued the call.
        // Appending user input first would interpose role:"user" and still 400.
        // resume() does not reconcile — pending approvals are re-raised, not voided.
        self.reconcile_tool_results(session).await?;
        // Slash skill dispatch: resolve /skill-name args BEFORE appending
        // so the raw text stays as the UserInput and the prepared body lands
        // as a durable SkillBody. SkillBody (not MetaUser) so the directive
        // survives a compaction boundary — compaction folds a MetaUser.
        let skill_meta = self.resolve_skill_slash(session, &user_input).await;
        self.append_user_input(session, user_input).await?;
        match skill_meta {
            crate::agent::skill_slash::SkillSlashOutcome::NotASkill => {}
            crate::agent::skill_slash::SkillSlashOutcome::Prepared {
                name,
                body,
                untrusted,
            } => {
                self.store
                    .append(new_event(
                        session,
                        TurnEventKind::SkillBody {
                            skill_name: name,
                            content: body,
                            agent_id: None,
                            untrusted,
                        },
                    ))
                    .await?;
            }
            crate::agent::skill_slash::SkillSlashOutcome::Refused(notice) => {
                // Surface the refusal to the user + end the turn without a
                // model call (the model has nothing to do for a refused
                // skill). The raw /-text stays as the UserInput so the
                // transcript shows what the user typed; the system line
                // explains the refusal.
                self.emit_system_line(notice);
                return Ok(RunResult {
                    outcome: RunOutcome::FinalOutput(String::new()),
                    turns: 0,
                    usage: Usage::default(),
                });
            }
        }
        // Turn-entry memory recall: scan the projected transcript for the
        // surfaced de-dup set, recall entries relevant to this turn's query,
        // and append a durable memory-recall attachment the projection merges
        // into this turn's user message. The system prompt stays byte-frozen
        // (memory is in the message stream, not the prompt) so prompt-cache
        // survives across turns. No-op when no memory provider is wired.
        self.inject_memory_recall(session).await?;
        // Turn-entry skill listing: announce model-invocable skills as a
        // system-reminder attachment the model reads to decide which skill
        // to invoke. Skips when a listing already survives in the served
        // view (first-turn announce, then no-op until compaction folds it
        // and the scan naturally resets). No-op when no registry is wired.
        self.inject_skill_listing_and_body(session).await?;
        let result = self.drive_loop(session, 0, Usage::default(), &token).await;
        // Notify any watcher the run reached a terminal state. On Ok the
        // status comes from the outcome; on Err the run failed. A spawned
        // child's bus bridge forwards this onto its completed topic.
        let result = match result {
            Ok(r) => {
                let (status, summary) = r.outcome.terminal_status();
                self.emit_run_completed(status, &summary);
                Ok(r)
            }
            Err(e) => {
                self.emit_run_completed("failed", &e.to_string());
                Err(e)
            }
        };
        // Persist extracted facts after the run. Failures are logged but
        // never fail the run — memory persistence is best-effort, not a
        // hard gate on the agent loop.
        if let Ok(_) = result
            && let Some(memory) = &self.memory
        {
            for entry in pending_facts {
                if let Err(e) = memory.add(entry) {
                    tracing::warn!("memory write failed: {e}");
                }
            }
        }
        result
    }

    /// Run on a session pre-seeded with a cloned event prefix (re-stamped
    /// to the forked session id) plus a user input. Used by the forked
    /// extraction runner: the main conversation is replayed into a fresh
    /// ephemeral session, the extraction prompt is the user input, drive_loop
    /// runs with the forked config. No fact extraction (the forked agent
    /// emits structured save-memory tool calls). The caller guarantees the
    /// prefix is consistent (forking at a stop boundary -- final response,
    /// no tool calls -- ensures no orphan ToolCall without a ToolResult).
    pub async fn run_forked(
        &self,
        session: SessionId,
        prefix: &[TurnEvent],
        user_input: String,
    ) -> Result<RunResult, RunError> {
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex") = Some(token.clone());
        for ev in prefix {
            self.store
                .append(new_event(session, ev.kind.clone()))
                .await?;
        }
        self.append_user_input(session, user_input).await?;
        self.drive_loop(session, 0, Usage::default(), &token).await
    }

    /// Continue a run paused on NextStep::Interruption. For each approval
    /// request the caller passes a decision for, the decision is applied:
    /// approved ⇒ execute the tool and append its result; rejected ⇒ append a
    /// rejection-note result. Pending approvals WITHOUT a matching decision are
    /// LEFT pending (no ToolResult appended) — the caller raises them one at a
    /// time. If any remain undecided, resume returns a fresh Interruption
    /// carrying the remainder so the caller shows the next approval dialog.
    /// Only when all have a decision does the loop resume (RunAgain). The
    /// ToolCall events are already in the log; resume only adds the matching
    /// ToolResults — no counter rewind (lossless log). The turn counter
    /// continues from the prior run (from the log) so max_turns is cumulative
    /// across run + resume; usage restarts at zero (not persisted).
    pub async fn resume(
        &self,
        session: SessionId,
        decisions: &[ApprovalDecision],
    ) -> Result<RunResult, RunError> {
        if let Some(r) = self.aborted_short_circuit(session).await? {
            // Abort path: Interrupted is terminal, but this skips drive_loop
            // (so the loop-exit finalize does not run). Finalize here.
            let result = Ok(r);
            self.finalize_input_buffer(&result);
            return result;
        }
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex") = Some(token.clone());
        let remaining = self.apply_decisions(session, decisions).await?;
        if !remaining.is_empty() {
            // Partial decision set: re-interrupt for the undecided calls so the
            // caller raises the next approval dialog. The decided calls already
            // have their ToolResults appended; only the undecided calls appear
            // here. turns is reported from the log so the cap stays cumulative.
            let prior_turns = self.count_turns(session).await?;
            self.mark_paused();
            return Ok(RunResult {
                outcome: RunOutcome::Interruption(remaining),
                turns: prior_turns,
                usage: Usage::default(),
            });
        }
        let prior_turns = self.count_turns(session).await?;
        self.drive_loop(session, prior_turns, Usage::default(), &token)
            .await
    }
}
