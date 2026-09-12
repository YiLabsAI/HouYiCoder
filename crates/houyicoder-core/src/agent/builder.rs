//! Configures Runner collaborators and runtime policy.

use std::path::PathBuf;
use std::sync::Arc;

use houyicoder_context::SessionId;
use houyicoder_resilience::resource_breaker::ResourceBreaker;

use super::conditional_activation::ConditionalSkillActivator;
use super::hook::registry::HookRegistry;
use super::memory::{MemoryRuntime, MutationLog};
use super::multi_agent::bus_types::BusMessage;
use super::skill_reload::SkillReloadGuard;
use super::tools::MemoryAddTool;
use super::{RunError, Runner, VerifyGate};
use houyicoder_api::agent_event::AgentEventHandlers;

impl Runner {
    /// Attach the aggregate resource breaker the sandbox enforces against, so
    /// status_snapshot reads the same breaker state. Consumes and returns self
    /// for chaining at the composition root (build_runner). The Arc is shared
    /// with the sandbox — both hold clones of the same breaker.
    pub fn with_breaker(mut self, breaker: Arc<ResourceBreaker>) -> Self {
        self.breaker = Some(breaker);
        self
    }

    /// Set the workspace used to discover project instructions.
    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.context_builder = self.context_builder.with_cwd(cwd);
        self
    }

    /// Switch the cwd at runtime through a shared Arc<Runner> (worktree
    /// enter/exit). Writes the interior-mutable cwd + clears the cached
    /// measurement so the next build recomputes the system prompt with
    /// the new project context.
    pub fn switch_cwd(&self, cwd: PathBuf) {
        self.context_builder.switch_cwd(cwd);
    }

    /// A shared handle to the interior-mutable cwd, for a WorktreeController
    /// that repoints the cwd without a typed Runner handle (writes the Arc
    /// directly through composition).
    pub fn cwd_handle(&self) -> std::sync::Arc<std::sync::RwLock<PathBuf>> {
        self.context_builder.cwd_handle()
    }

    /// Install an optional post-run verification gate. After a run
    /// reaches FinalOutput the runner calls gate.verify before
    /// returning; a failed verify surfaces RunOutcome::VerifyFailed so the
    /// caller can re-prompt the model to fix its own work. None
    /// (the default) means no gate — FinalOutput passes through
    /// unchanged. Consumes and returns self for chaining at the
    /// composition root.
    pub fn with_verify_gate(mut self, gate: Arc<dyn VerifyGate>) -> Self {
        self.verify_gate = Some(gate);
        self
    }

    /// Install all event-domain handlers before the runner is shared.
    pub fn set_event_handlers(&mut self, events: AgentEventHandlers) {
        self.memory.set_event_handlers(&events);
        self.events = events;
    }

    /// Install the bus inbox receiver for a spawned child. Call before the
    /// runner is shared. The drive loop drains this at each turn boundary, appending Inbox texts as user
    /// messages so a parent can steer a running child mid-task.
    pub fn set_inbox(&mut self, rx: tokio::sync::mpsc::UnboundedReceiver<BusMessage>) {
        *self.inbox.lock().expect("inbox lock") = Some(rx);
    }

    /// Clone the installed event handlers for a staged collaborator.
    pub fn event_handlers(&self) -> AgentEventHandlers {
        self.events.clone()
    }

    /// Install a fully constructed memory runtime. Registers the
    /// save_memory tool sharing the runtime's provider so main-agent saves
    /// and forked-extract saves land under the same lock. Consumes and
    /// returns self for chaining at the composition root.
    pub fn install_memory(mut self, runtime: MemoryRuntime) -> Self {
        runtime.set_event_handlers(&self.events);
        if let Some(provider) = runtime.provider() {
            self.tools.register(Arc::new(
                MemoryAddTool::new(provider.clone())
                    .with_origin(houyicoder_context::MemoryOrigin::MainAgent),
            ));
        }
        self.memory = runtime;
        self
    }

    /// Install memory writes and mutation tracking for an extraction run.
    pub(crate) fn install_extraction_memory(
        mut self,
        provider: Arc<dyn houyicoder_api::memory::MemoryProvider>,
        recorder: Arc<MutationLog>,
    ) -> Self {
        self.tools.register(Arc::new(
            MemoryAddTool::new(provider.clone())
                .with_recorder(recorder)
                .with_origin(houyicoder_context::MemoryOrigin::Extractor),
        ));
        self.memory.install_provider(provider);
        self
    }

    /// Read access to the tool registry. The composition root + tests use it
    /// to assert a tool is registered (e.g. save_memory after install_memory).
    pub fn tools(&self) -> &super::ToolRegistry {
        &self.tools
    }

    /// Install the hook registry. When set, the runner fires PreToolUse before
    /// each tool execution and PostToolUse / PostToolUseFailure after, then
    /// arbitrates the verdicts to drive flow control (Deny blocks the call,
    /// Feedback surfaces a self-correction signal to the model, Observe is
    /// logged, Trigger fires a downstream event, Allow proceeds). None (the
    /// default) means no hooks fire at runtime. Consumes and returns self
    /// for chaining at the composition root, where settings-loaded hooks
    /// register before the runner is shared.
    pub fn with_hooks(mut self, hooks: Arc<HookRegistry>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Install the skill-hook registrar shared by skill activation paths.
    /// dispatch. The registrar holds a live workspace-trust ref the server
    /// writes after the startup trust prompt resolves; both invocation paths
    /// call its register method after a skill body prepares. Unwired in tests
    /// and the pure-stub path: no skill hooks register and set_trust is a
    /// no-op.
    pub fn with_registrar(mut self, registrar: Arc<crate::agent::SkillHookRegistrar>) -> Self {
        self.registrar = Some(registrar);
        self
    }

    /// Install the path-gated skill activator used by file tools
    /// and the listing. None when off; the listing then shows every skill.
    pub fn with_conditional(mut self, conditional: Arc<dyn ConditionalSkillActivator>) -> Self {
        self.conditional = Some(conditional);
        self
    }

    /// Hold a hot-reload driver lifetime guard. The guard is never read;
    /// dropping it (with the runner) stops the watcher + thread. None means
    /// no hot-reload driver runs for this session.
    /// Set the hot-reload driver lifetime guard after construction (the
    /// builder chain ends before the reloader can be built, so the
    /// composition root sets it here). None when no driver was constructed.
    pub fn set_skill_reloader(&mut self, reloader: Option<Arc<dyn SkillReloadGuard>>) {
        self.skill_reloader = reloader;
    }

    /// Write the resolved workspace trust through the registrar. The server
    /// calls this once after the startup trust prompt so a Project or Local
    /// skill hook invoked later reads the resolved value, not the
    /// fail-closed default. No-op without a registrar.
    pub fn set_trust(&self, state: houyicoder_api::trust::TrustState) {
        if let Some(r) = self.registrar.as_ref() {
            r.set_trust(state);
        }
    }

    /// Override the cache policy (defaults to the Auto three-breakpoint set).
    /// A provider with no prompt-cache support swaps NoCachePolicy; a
    /// config-driven explicit policy lands when that wires.
    pub fn with_cache_policy(
        mut self,
        policy: Arc<dyn houyicoder_api::cache_policy::CachePolicyProvider>,
    ) -> Self {
        self.cache_policy = policy;
        self
    }

    /// Override the per-provider cost model (defaults to the Anthropic-pricing
    /// rates). A multi-provider runtime swaps per active provider so the
    /// economy-driven compaction gate uses the right cache-read/write ratios.
    pub fn with_cost_model(
        mut self,
        model: Arc<dyn houyicoder_api::cost_model::CostModelProvider>,
    ) -> Self {
        self.cost_model = model;
        self
    }

    /// A shared handle to the recall meter, so the composition root can pass
    /// the same Arc to the conversation recall tool (the tool bumps it on a
    /// match in the folded span) and the compaction path (it snapshots +
    /// resets to compute the recall rate). The composition root registers the
    /// tool with this handle so the tool + the runner share one counter.
    pub fn recall_meter(&self) -> Arc<std::sync::atomic::AtomicU32> {
        Arc::clone(&self.recall_meter)
    }

    /// Install a workspace probe for the re-derivable compaction backbone's
    /// derivation watermark. The composition root passes a GitWorkspaceProbe
    /// sharing the cwd handle so worktree switches propagate. Called pre-Arc,
    /// before sharing the runner. None (the default) means the backbone runs the
    /// log-rederivable layer only; the workspace watermark fields are None.
    pub fn set_workspace_probe(&mut self, probe: Arc<dyn super::backbone::WorkspaceProbe>) {
        self.workspace_probe = Some(probe);
    }

    /// Install a tool-output reducer. The isolate stage reduces a large tool
    /// result (strip ansi, head/tail, truncate) before serving it so the
    /// served preview is compact; the raw stays in the CAS. None (the
    /// default) ⇒ no reduction (the raw preview is served). The composition
    /// root wires a HotPathReducer for the built-in tools.
    pub fn with_reducer(mut self, reducer: Arc<dyn super::reducer::ToolOutputReducer>) -> Self {
        self.reducer = Some(reducer);
        self
    }

    /// Override the default recall meter with one the composition root
    /// constructed + shared with the conversation recall tool at
    /// registration time. The tool + the compaction path must share one
    /// Arc so the tool's bumps land on the counter the compaction path
    /// snapshots. The composition root constructs the meter, passes clones
    /// to the tool (at registration) + here (before the runner is shared),
    /// so both see the same counter.
    pub fn with_recall_meter(mut self, meter: Arc<std::sync::atomic::AtomicU32>) -> Self {
        self.recall_meter = meter;
        self
    }

    /// Install the skill registry used to build the
    /// skill-discovery listing attachment. The registry is discovered at
    /// startup; the same Arc is shared with the Skill tool (registered
    /// separately at the composition root) so invocation + listing see one
    /// set. None in tests and the pure-stub path (no listing attached).
    pub fn with_skill_registry(
        mut self,
        registry: Arc<dyn houyicoder_api::skill::SkillRegistry>,
    ) -> Self {
        self.skill_registry = Some(registry);
        self
    }

    /// Queue startup warnings (bad settings fields, network policy typos)
    /// for the host to drain + surface as initial transcript system lines.
    /// A bad settings value must not silently become a no-op; these land
    /// synchronously at pair time (no async-sink race).
    pub fn with_startup_warnings(self, warnings: Vec<String>) -> Self {
        if let Ok(mut g) = self.startup_warnings.lock() {
            g.extend(warnings);
        }
        self
    }

    /// Delegate the served-models refresh to the provider. The host spawns
    /// this on the runtime at startup, fire-and-forget. The default provider
    /// impl is a no-op; the OpenAI-compatible impl fetches /v1/models and
    /// writes the cache.
    pub fn refresh_served_models(
        &self,
    ) -> houyicoder_async::PFut<'_, Result<(), houyicoder_protocol::llm::ProviderError>> {
        self.provider.refresh_served_models()
    }

    /// Await in-flight dream tasks (reward-dream or consolidation) until
    /// they finish or the timeout expires. Tests use this instead of
    /// polling dream_count on a sleep loop — the JoinHandle await is
    /// event-driven (the scheduler wakes on task completion), and the
    /// deadline is a safety bound, not a poll interval.
    pub async fn join_dreams(&self, timeout: std::time::Duration) {
        self.memory.join_background(timeout).await;
    }

    /// Auto compaction (fired by the pre-flight and overflow handlers): fold
    /// older events into a summary, persist a checkpoint manifest, append
    /// CompactionBoundary + Summary events, and fire hooks. Delegates to
    /// run_compaction so manual and auto share one pipeline. Returns true
    /// when progress was made (at least one Summarized event); false means
    /// the manifest is all-Verbatim and the caller must fail-closed.
    pub async fn compress(&self, session: SessionId) -> Result<bool, RunError> {
        // Compaction rewrites the prefix, so the next provider response will
        // likely show a cache-read drop. Cleared in append_turn_usage.
        self.cache_compact_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let outcome = self
            .run_compaction(session, crate::agent::hook::CompactTrigger::Auto)
            .await?;
        Ok(outcome.made_progress)
    }

    /// Before-clear preservation: scan the whole session for unsolved-problem
    /// and key-decision signals, write them to the auto scope so key facts
    /// survive /clear. Best-effort: a write failure logs and continues;
    /// memory never blocks the clear path. No-op when no memory provider
    /// is available.
    pub async fn before_clear(&self, session: SessionId) -> Result<(), RunError> {
        self.memory.preserve_before_clear(session).await
    }

    /// Override the default heuristic summarizer with an LLM-backed one. The
    /// composition root installs the production implementation.
    pub fn with_summarizer(mut self, summarizer: Box<dyn super::manifest::Summarizer>) -> Self {
        self.summarizer = summarizer;
        self
    }

    /// Override snapshot retention (TTL seconds + size cap bytes). The
    /// composition root calls this to tune pruning; defaults are seven days and
    /// one gibibyte.
    pub fn with_snapshot_retention(mut self, ttl_secs: u64, size_cap_bytes: u64) -> Self {
        self.snapshot_ttl_secs = ttl_secs;
        self.snapshot_size_cap_bytes = size_cap_bytes;
        self
    }

    /// Format the memory index for the system prompt prefix. Returns None
    /// without a provider or the store is empty. Capped at 200
    /// entries.
    pub fn format_memory_index(&self) -> Option<String> {
        self.memory.format_index()
    }

    /// Token count of the compact summary text (the replacement for folded
    /// turns). Returns 0 when no compaction has run or the summary is empty.
    /// Used to populate a "Compact buffer" category in /context.
    pub async fn compact_summary_tokens(&self, session: SessionId) -> u32 {
        let Ok(view) = self.store.current_view(session).await else {
            return 0;
        };
        let Some(manifest) = &view.manifest else {
            return 0;
        };
        let Some(summary) = &manifest.summary else {
            return 0;
        };
        self.context_builder.tokenizer().count(summary)
    }

    /// Format a compact summary for the /context view. Returns None when no
    /// compaction has run. Counts folded turn groups + truncates the summary
    /// preview to one line.
    pub async fn compact_summary(&self, session: SessionId) -> Option<String> {
        let view = self.store.current_view(session).await.ok()?;
        let compact_count = view.rewind_points.len();
        if compact_count == 0 {
            return None;
        }
        let manifest = view.manifest.as_ref()?;
        let folded: usize = manifest
            .plan
            .iter()
            .filter(|g| matches!(g.disposition, houyicoder_context::Disposition::Summarized))
            .count();
        let summary_preview = truncate_summary_preview(manifest.summary.as_deref());
        Some(format!(
            "{compact_count} compacts · {folded} turns folded · {summary_preview}"
        ))
    }
}

/// Truncate a compact summary to a one-line preview for /context. Takes the
/// first line + caps it at 80 chars, appending an ellipsis when truncated.
/// Char-safe (char_indices) so a multi-byte boundary never panics -- the
/// prior byte slice at 80 would panic on a CJK/emoji summary whose 80th
/// byte landed mid-char. None in -> empty preview (no summary yet).
fn truncate_summary_preview(summary: Option<&str>) -> String {
    let Some(s) = summary else {
        return String::new();
    };
    let line = s.lines().next().unwrap_or("");
    // char_indices().nth(80) is the byte offset of the 81st char; if present,
    // the line has >80 chars and we slice at that char boundary (safe).
    match line.char_indices().nth(80) {
        Some((boundary, _)) => format!("\"{}…\"", &line[..boundary]),
        None => format!("\"{line}\""),
    }
}

#[cfg(test)]
mod cache_policy_tests {
    use super::*;
    use houyicoder_api::cache_policy::{AutoCachePolicy, CachePolicy, NoCachePolicy};
    use houyicoder_api::cost_model::AnthropicCostModel;

    fn runner() -> Runner {
        Runner::new(
            std::sync::Arc::new(houyicoder_session::SessionStore::new(Box::new(
                houyicoder_memory::InMemoryBackend::new(),
            ))),
            std::sync::Arc::new(crate::provider::test_support::FakeProvider::text("x")),
            crate::agent::ToolRegistry::new(),
            crate::agent::runner_config::RunnerConfig::default(),
        )
    }

    #[test]
    fn test_default_runner_uses_policy() {
        let r = runner();
        assert_eq!(r.cache_policy.policy(), CachePolicy::Auto);
    }

    #[test]
    fn test_with_cache_policy_overrides() {
        // with_cache_policy swaps the default Auto for NoCachePolicy. Covers
        // the setter (otherwise dead in the default-Auto path).
        let r = runner().with_cache_policy(std::sync::Arc::new(NoCachePolicy));
        assert_eq!(r.cache_policy.policy(), CachePolicy::None);
        // Auto round-trips too.
        let r2 = runner().with_cache_policy(std::sync::Arc::new(AutoCachePolicy));
        assert_eq!(r2.cache_policy.policy(), CachePolicy::Auto);
    }

    #[test]
    fn test_with_cost_model_overrides() {
        // with_cost_model swaps the default Anthropic rates. Covers the
        // setter (otherwise dead in the default-cost path).
        let r = runner().with_cost_model(std::sync::Arc::new(AnthropicCostModel));
        let cost = r.cost_model.cost();
        assert!(
            (cost.cache_read - 0.1).abs() < 1e-9,
            "Anthropic cache_read 0.1x"
        );
    }

    /// The compact summary preview truncates at 80 chars. The truncation is
    /// char-safe (char_indices) so a multi-byte CJK/emoji summary whose 80th
    /// byte landed mid-char does not panic -- the prior byte slice did.
    #[test]
    fn test_truncate_summary_preview_safe() {
        // ASCII > 80 chars: truncated + ellipsis + quoted.
        let long = "x".repeat(100);
        let p = truncate_summary_preview(Some(&long));
        assert!(p.contains('…') && p.starts_with('"'), "truncated: {p}");
        // Short: no truncation.
        assert_eq!(truncate_summary_preview(Some("short")), "\"short\"");
        // None: empty preview.
        assert_eq!(truncate_summary_preview(None), "");
        // CJK > 80 chars (= 300 bytes): truncates WITHOUT panic (char-safe).
        // The prior byte slice at 80 would panic here (byte 80 is mid-char).
        let cjk = "字".repeat(100);
        let p = truncate_summary_preview(Some(&cjk));
        assert!(p.contains('…'), "CJK truncated without panic: {p}");
    }
}

#[cfg(test)]
mod compact_summary_tests {
    use super::*;
    use houyicoder_context::{
        CheckpointId, CheckpointManifest, Disposition, EventId, SessionEvent, SessionId,
        SessionLogEntry, TurnGroup,
    };
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_session::SessionStore;

    fn runner_with_store() -> (Runner, SessionId) {
        let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let session = SessionId::new();
        let runner = Runner::with_shared_store(
            store,
            Arc::new(crate::provider::test_support::FakeProvider::new(vec![])),
            crate::agent::ToolRegistry::new(),
            crate::agent::runner_config::RunnerConfig::default(),
        );
        (runner, session)
    }

    #[tokio::test]
    async fn test_compact_summary_no_checkpoint() {
        let (runner, session) = runner_with_store();
        assert_eq!(runner.compact_summary(session).await, None);
    }

    #[tokio::test]
    async fn test_compact_summary_formats_manifest() {
        let (runner, session) = runner_with_store();
        let event = SessionLogEntry {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::AssistantMessage {
                text: "folded".into(),
                thinking: None,
            },
        };
        runner.store().append(event.clone()).await.unwrap();
        let manifest = CheckpointManifest {
            id: CheckpointId::new(),
            session,
            last_event: event.id,
            summary: Some("folded earlier turns".into()),
            plan: vec![TurnGroup {
                turn_id: event.id,
                disposition: Disposition::Summarized,
                event_ids: vec![event.id],
            }],
            ts: 0,
        };
        runner
            .store()
            .backend()
            .write_checkpoint(manifest)
            .await
            .unwrap();
        let summary = runner.compact_summary(session).await;
        assert!(summary.is_some(), "compact summary should be Some");
        let s = summary.unwrap();
        assert!(s.contains("1 compacts"), "{s}");
        assert!(s.contains("1 turns folded"), "{s}");
        assert!(s.contains("folded earlier turns"), "{s}");
    }

    #[tokio::test]
    async fn test_compact_tokens_no_checkpoint() {
        let (runner, session) = runner_with_store();
        assert_eq!(runner.compact_summary_tokens(session).await, 0);
    }

    #[tokio::test]
    async fn test_compact_summary_tokens_none() {
        let (runner, session) = runner_with_store();
        let event = SessionLogEntry {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::AssistantMessage {
                text: "folded".into(),
                thinking: None,
            },
        };
        runner.store().append(event.clone()).await.unwrap();
        let manifest = CheckpointManifest {
            id: CheckpointId::new(),
            session,
            last_event: event.id,
            summary: None,
            plan: vec![TurnGroup {
                turn_id: event.id,
                disposition: Disposition::Summarized,
                event_ids: vec![event.id],
            }],
            ts: 0,
        };
        runner
            .store()
            .backend()
            .write_checkpoint(manifest)
            .await
            .unwrap();
        assert_eq!(
            runner.compact_summary_tokens(session).await,
            0,
            "None summary -> 0 tokens"
        );
    }

    #[tokio::test]
    async fn test_compact_summary_tokens_counts() {
        let (runner, session) = runner_with_store();
        let event = SessionLogEntry {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::AssistantMessage {
                text: "folded".into(),
                thinking: None,
            },
        };
        runner.store().append(event.clone()).await.unwrap();
        let manifest = CheckpointManifest {
            id: CheckpointId::new(),
            session,
            last_event: event.id,
            summary: Some("this is a summary of folded turns".into()),
            plan: vec![TurnGroup {
                turn_id: event.id,
                disposition: Disposition::Summarized,
                event_ids: vec![event.id],
            }],
            ts: 0,
        };
        runner
            .store()
            .backend()
            .write_checkpoint(manifest)
            .await
            .unwrap();
        let tokens = runner.compact_summary_tokens(session).await;
        assert!(
            tokens > 0,
            "summary text produces non-zero tokens: {tokens}"
        );
    }

    #[test]
    fn test_memory_index_without_provider() {
        let (runner, _) = runner_with_store();
        assert_eq!(runner.format_memory_index(), None);
    }

    #[test]
    fn test_memory_index_formats_entries() {
        use crate::agent::MemoryRuntime;
        use houyicoder_api::memory::MemoryProvider;
        use houyicoder_context::{MemoryEntry, MemorySource};
        use houyicoder_memory::MarkdownMemoryProvider;
        let root =
            std::env::temp_dir().join(format!("mem-index-{}-{}", std::process::id(), line!()));
        drop(std::fs::remove_dir_all(&root));
        std::fs::create_dir_all(&root).expect("mkdir");
        let memory: Arc<dyn MemoryProvider> = Arc::new(MarkdownMemoryProvider::new(root.clone()));
        memory
            .add(MemoryEntry::new(
                "proj-pref",
                "prefer let chains",
                MemorySource::Project,
            ))
            .unwrap();
        let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let mut runtime = MemoryRuntime::new(store.clone());
        runtime.install_provider(memory);
        let runner = Runner::with_shared_store(
            store,
            Arc::new(crate::provider::test_support::FakeProvider::new(vec![])),
            crate::agent::ToolRegistry::new(),
            crate::agent::runner_config::RunnerConfig::default(),
        )
        .install_memory(runtime);
        let idx = runner.format_memory_index();
        assert!(idx.is_some(), "configured provider produces an index");
        let s = idx.unwrap();
        assert!(s.contains("proj-pref"), "key in index: {s}");
        assert!(s.contains("project"), "source label: {s}");
        assert!(s.contains("prefer let chains"), "description: {s}");
        drop(std::fs::remove_dir_all(&root));
    }
}

#[cfg(test)]
#[path = "reward_feed_tests.rs"]
mod reward_feed_tests;
