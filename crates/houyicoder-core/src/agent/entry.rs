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
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32};
use std::sync::{Arc, Mutex, RwLock};

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
        let active_model = Arc::new(RwLock::new(config.model.clone()));
        let active_effort = Arc::new(RwLock::new(None));
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
            inbox: Mutex::new(None),
            startup_warnings: Mutex::new(Vec::new()),
            breaker: None,
            usage: Arc::new(Mutex::new(UsageAccumulator::default())),
            observability,
            cancel: Mutex::new(None),
            aborted: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            turn_cancel: Mutex::new(None),
            verify_gate: None,
            undo_stack: None,
            snapshot_store: None,
            snapshot_ttl_secs: DEFAULT_SNAPSHOT_TTL_SECS,
            snapshot_size_cap_bytes: DEFAULT_SNAPSHOT_SIZE_CAP_BYTES,
            summarizer: Box::new(manifest::HeuristicSummarizer),
            memory: None,
            skill_registry: None,
            sandbox_session: None,
            skill_grants: None,
            active_skill: Arc::new(Mutex::new(None)),
            hooks: None,
            registrar: None,
            conditional: None,
            skill_reloader: None,
            cache_policy: Arc::new(houyicoder_api::cache_policy::AutoCachePolicy),
            cost_model: Arc::new(houyicoder_api::cost_model::AnthropicCostModel),
            recall_meter: Arc::new(AtomicU32::new(0)),
            workspace_probe: None,
            compact_suppress: AtomicU8::new(0),
            compact_consecutive_failures: AtomicU32::new(0),
            cache_prev_read: Mutex::new(None),
            cache_compact_flag: AtomicBool::new(false),
            cache_model_switch_flag: AtomicBool::new(false),
            cached_prefix: Arc::new(cache_liveness::CachedPrefixState::new()),
            reducer: None,
            extractor: None,
            dream: None,
            auto_memory: Arc::new(AtomicBool::new(true)),
            auto_dream: Arc::new(AtomicBool::new(true)),
            queued_input: Mutex::new(VecDeque::new()),
            queued_notifications: Mutex::new(VecDeque::new()),
            consumed_input: Mutex::new(Vec::new()),
            redundancy: Mutex::new(redundancy::RedundancyTracker::new()),
            denied_agents: Arc::new(HashSet::new()),
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
        self.prune_snapshots();
        self.reset_run_state();
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
                self.emit_system_line(notice);
                return Ok(RunResult {
                    outcome: RunOutcome::FinalOutput(String::new()),
                    turns: 0,
                    usage: Usage::default(),
                });
            }
        }
        self.inject_memory_recall(session).await?;
        self.inject_skill_listing_and_body(session).await?;
        let result = self.drive_loop(session, 0, Usage::default(), &token).await;
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
        // Best-effort fact persistence: failures are logged, not fatal.
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

    /// Run on a session pre-seeded with a cloned event prefix plus a user
    /// input. Used by the forked extraction runner. The caller guarantees
    /// the prefix ends at a stop boundary (no orphan ToolCall).
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
        if let Some(r) = self.aborted_short_circuit(session).await? {
            // Abort skips drive_loop, so finalize here.
            let result = Ok(r);
            self.finalize_input_buffer(&result);
            return result;
        }
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex") = Some(token.clone());
        let remaining = self.apply_decisions(session, decisions).await?;
        if !remaining.is_empty() {
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
}

/// Extracted as a free function so the logic is testable without a Runner.
fn reapply_skill_entitlements_impl(
    active: Option<String>,
    session: Option<&dyn houyicoder_api::sandbox::SandboxSession>,
    registry: Option<&dyn houyicoder_api::skill::SkillRegistry>,
    grants: Option<&houyicoder_api::skill::grant::SkillGrantStore>,
) {
    if let Some(name) = active
        && let Some(session) = session
        && let Some(registry) = registry
    {
        let desc = registry.find(&name);
        let source = registry.source_for(&name);
        let (mach, allow_launch) = houyicoder_api::skill::grant::resolve_entitlements(
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
mod tests {
    use super::*;
    use houyicoder_api::sandbox::SandboxSession;
    use houyicoder_api::skill::grant::SkillGrantStore;
    use houyicoder_api::skill::{
        SkillDescriptor, SkillError, SkillFamily, SkillProvenance, SkillRegistry, SkillSource,
    };
    use houyicoder_async::PFut;
    use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// A sandbox session that records entitlement grants and clears.
    struct RecordingSession {
        mach: Mutex<Vec<String>>,
        app_launch: Mutex<bool>,
        cleared: Mutex<u32>,
    }
    impl RecordingSession {
        fn new() -> Self {
            Self {
                mach: Mutex::new(Vec::new()),
                app_launch: Mutex::new(false),
                cleared: Mutex::new(0),
            }
        }
    }
    impl SandboxSession for RecordingSession {
        fn exec_with_config(
            &self,
            _: &str,
            _: ExecConfig,
        ) -> PFut<'_, Result<ExecResult, SandboxError>> {
            Box::pin(async { Err(SandboxError::Unsupported("test".into())) })
        }
        fn workspace_root(&self) -> Arc<Path> {
            Arc::from(PathBuf::from("/"))
        }
        fn set_extra_mach_services(&self, services: &[String]) {
            let mut m = self.mach.lock().unwrap();
            m.clear();
            m.extend_from_slice(services);
        }
        fn grant_app_launch(&self) {
            *self.app_launch.lock().unwrap() = true;
        }
        fn clear_skill_grants(&self) {
            *self.cleared.lock().unwrap() += 1;
            self.mach.lock().unwrap().clear();
            *self.app_launch.lock().unwrap() = false;
        }
    }

    /// A registry whose skill declares a mach service + app launch.
    struct EntitlementRegistry;
    impl SkillRegistry for EntitlementRegistry {
        fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
            Vec::new()
        }
        fn find(&self, name: &str) -> Option<SkillDescriptor> {
            (name == "ego-browser").then(|| SkillDescriptor {
                name: "ego-browser".to_string(),
                description: "d".into(),
                when_to_use: None,
                argument_hint: None,
                disable_model_invocation: false,
                user_invocable: true,
                body_token_estimate: 0,
                allowed_tools: Vec::new(),
                allowed_mach_services: vec!["com.citrolabs.ego.lite.ego-browser".into()],
                allow_app_launch: true,
            })
        }
        fn source_for(&self, name: &str) -> Option<SkillSource> {
            (name == "ego-browser")
                .then(|| SkillSource::new(SkillFamily::Agents, SkillProvenance::UserHome))
        }
        fn prepare_body(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<String, SkillError> {
            Ok("body".into())
        }
    }

    /// When a skill is active, reapply re-grants its mach services and app
    /// launch from the frontmatter/profile, so a later-turn bash command can
    /// reach the ego bootstrap.
    #[test]
    fn test_reapply_carries_active_skill() {
        let session = RecordingSession::new();
        reapply_skill_entitlements_impl(
            Some("ego-browser".into()),
            Some(&session),
            Some(&EntitlementRegistry),
            None,
        );
        let mach = session.mach.lock().unwrap().clone();
        assert_eq!(
            mach,
            vec!["com.citrolabs.ego.lite.ego-browser".to_string()],
            "mach services re-granted for carried skill"
        );
        assert!(
            *session.app_launch.lock().unwrap(),
            "app launch re-granted for carried skill"
        );
    }

    /// When no skill is active, reapply clears the slate so a prior skill's
    /// grants do not leak into an unrelated run.
    #[test]
    fn test_reapply_clears_without_skill() {
        let session = RecordingSession::new();
        reapply_skill_entitlements_impl(None, Some(&session), Some(&EntitlementRegistry), None);
        assert!(
            session.mach.lock().unwrap().is_empty(),
            "mach cleared when no skill active"
        );
        assert!(
            !*session.app_launch.lock().unwrap(),
            "app launch cleared when no skill active"
        );
        assert!(
            *session.cleared.lock().unwrap() > 0,
            "clear_skill_grants called"
        );
    }

    /// A grant store with a user-approved service feeds into reapply, so a
    /// denial approved in a prior turn takes effect on the next turn.
    #[test]
    fn test_reapply_picks_up_grant() {
        let session = RecordingSession::new();
        let grants = SkillGrantStore::with_path(std::env::temp_dir().join(format!(
            "houyi-reapply-grants-{}-{}.json",
            std::process::id(),
            1
        )));
        let source = SkillSource::new(SkillFamily::Agents, SkillProvenance::UserHome);
        grants
            .add_grants(
                &source.grant_subject("ego-browser"),
                vec!["com.test.extra.service".into()],
            )
            .unwrap();
        reapply_skill_entitlements_impl(
            Some("ego-browser".into()),
            Some(&session),
            Some(&EntitlementRegistry),
            Some(&grants),
        );
        let mach = session.mach.lock().unwrap().clone();
        assert!(
            mach.contains(&"com.test.extra.service".to_string()),
            "user-approved grant picked up on reapply: {mach:?}"
        );
        assert!(
            mach.contains(&"com.citrolabs.ego.lite.ego-browser".to_string()),
            "profile service still present: {mach:?}"
        );
    }

    /// Exercise the stub trait methods so diff-cov sees them. The stub
    /// session and registry implement required trait methods that the
    /// reapply path does not call; this test touches them once.
    #[test]
    fn test_stubs_are_callable() {
        let session = RecordingSession::new();
        let _root = session.workspace_root();
        let reg = EntitlementRegistry;
        assert!(reg.list_model_invocable().is_empty());
        assert!(reg.prepare_body("x", None, None).is_ok());
    }
}
