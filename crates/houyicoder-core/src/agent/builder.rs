//! Composition-root wiring: builder methods that attach the shared breaker,
//! pin the context cwd, install the verify gate, and set the live-event sink.
//! Split from mod.rs so the runner surface stays under the file-size gate;
//! these are pure field setters consumed at the composition root (build_runner).

use std::path::PathBuf;
use std::sync::Arc;

use houyicoder_context::SessionId;
use houyicoder_resilience::resource_breaker::ResourceBreaker;

use super::{RunError, Runner, VerifyGate};
use houyicoder_api::live::LiveSink;

impl Runner {
    /// Attach the aggregate resource breaker the sandbox enforces against, so
    /// status_snapshot reads the same breaker state. Consumes and returns self
    /// for chaining at the composition root (build_runner). The Arc is shared
    /// with the sandbox — both hold clones of the same breaker.
    pub fn with_breaker(mut self, breaker: Arc<ResourceBreaker>) -> Self {
        self.breaker = Some(breaker);
        self
    }

    /// Pin the cwd the context builder uses for the project-context walk-up
    /// (the nearest AGENTS.md). Call at the composition root with the resolved
    /// workspace root so the system prompt's project-context section reads the
    /// right file; tests default to the process cwd. Consumes and returns self
    /// for chaining.
    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.context_builder = self.context_builder.with_cwd(cwd);
        self
    }

    /// Switch the cwd at runtime through a shared Arc<Runner> (worktree
    /// enter/exit). Writes the interior-mutable cwd + clears the cached served
    /// view so the next build recomputes the system prompt with the new
    /// project context.
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

    /// Install a live-event sink. Call before the runner is shared (Arc-ed) —
    /// the host owns the value, sets the sink, then wraps it. The closure
    /// receives each LiveEvent the runner emits during a streaming turn.
    ///
    /// Also forwards the sink to the extractor + dream so a background pass
    /// that writes memories can push one MemorySaved event (event-driven push,
    /// not a second channel). The forked runners inside extract/dream do not
    /// get the sink — they have their own None live_sink — so the fork's token
    /// deltas never fire into the user transcript.
    pub fn set_live_sink(&mut self, sink: LiveSink) {
        if let Some(e) = self.extractor.as_ref() {
            e.set_notify_sink(sink.clone());
        }
        if let Some(d) = self.dream.as_ref() {
            d.set_notify_sink(sink.clone());
        }
        self.live = Some(sink);
    }

    /// Clone the installed live sink, if any. Lets the composition root share
    /// the runner's sink with collaborators built before the runner (the
    /// worktree controller) without a second channel.
    pub fn live_sink(&self) -> Option<LiveSink> {
        self.live.clone()
    }

    /// Wire a persistent memory provider. When set, the turn-entry step
    /// recalls relevant entries per user query and appends a durable
    /// memory-recall attachment the projection merges into the turn's user
    /// message — the system prompt stays byte-frozen across turns
    /// (prompt-cache friendly), and compaction folds old memory-recall events
    /// out of the view so the surfaced set naturally resets. After a run
    /// completes, explicit save signals in the user input are written
    /// atomically. Consumes and returns self for chaining at the composition
    /// root.
    pub fn with_memory(
        mut self,
        provider: Arc<dyn houyicoder_api::memory::MemoryProvider>,
    ) -> Self {
        // Register the structured save_memory tool so the main agent can save
        // a memory by emitting a save_memory tool call — auto-approve, no
        // path-escape surface (the provider owns every path), routes through
        // the provider atomic write under the in-process lock. The tool
        // shares the provider handle so main-agent saves and forked-extract
        // saves land under the same lock. Registering here (not at the
        // composition root) keeps the tool coupled to memory being wired and
        // covers the forked-extract runner for free (it also calls
        // with_memory).
        self.tools.register(Arc::new(
            super::tools::memory_add::MemoryAddTool::new(provider.clone())
                .with_origin(houyicoder_context::MemoryOrigin::MainAgent),
        ));
        self.memory = Some(provider);
        self
    }

    /// Like with_memory but registers save_memory with a write counter the
    /// caller threads in. The forked extraction runner uses this so it can
    /// read how many saves landed this pass + fire one memory-saved notice.
    /// The main runner uses with_memory (no counter; it does not notify).
    pub fn with_memory_counted(
        mut self,
        provider: Arc<dyn houyicoder_api::memory::MemoryProvider>,
        counter: Arc<std::sync::atomic::AtomicU32>,
    ) -> Self {
        self.tools.register(Arc::new(
            super::tools::memory_add::MemoryAddTool::new(provider.clone())
                .with_counter(counter)
                .with_origin(houyicoder_context::MemoryOrigin::Extractor),
        ));
        self.memory = Some(provider);
        self
    }

    /// Read access to the tool registry. The composition root + tests use it
    /// to assert a tool is registered (e.g. save_memory after with_memory).
    pub fn tools(&self) -> &super::ToolRegistry {
        &self.tools
    }

    /// Wire the hook registry. When set, the runner fires PreToolUse before
    /// each tool execution and PostToolUse / PostToolUseFailure after, then
    /// arbitrates the verdicts to drive flow control (Deny blocks the call,
    /// Feedback surfaces a self-correction signal to the model, Observe is
    /// logged, Trigger fires a downstream event, Allow proceeds). None (the
    /// default) means no hooks fire at runtime. Consumes and returns self
    /// for chaining at the composition root, where settings-loaded hooks
    /// register before the runner is shared.
    pub fn with_hooks(mut self, hooks: Arc<crate::agent::hook::registry::HookRegistry>) -> Self {
        self.hooks = Some(hooks);
        self
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

    /// Wire a workspace probe for the re-derivable compaction backbone's
    /// derivation watermark. The composition root passes a GitWorkspaceProbe
    /// sharing the cwd handle so worktree switches propagate. Called pre-Arc,
    /// like set_live_sink. None (the default) ⇒ the backbone runs the
    /// log-rederivable layer only; the workspace watermark fields are None.
    pub fn set_workspace_probe(&mut self, probe: Arc<dyn super::backbone::WorkspaceProbe>) {
        self.workspace_probe = Some(probe);
    }

    /// Wire a tool-output reducer. the isolate stage reduces a large tool
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

    /// Wire the memory extractor that fires at query-loop end to
    /// background-extract memories from the conversation. Fire-and-forget:
    /// the spawned fork runs on a tokio task so the main loop is never
    /// blocked. Only the main runner wires this; the forked extraction
    /// runner leaves it None so it never recursively self-triggers.
    pub fn with_extractor(
        mut self,
        extractor: Arc<crate::agent::extractor::MemoryExtractor>,
    ) -> Self {
        self.extractor = Some(extractor);
        self
    }

    /// Wire the consolidation dream that fires at query-loop end to
    /// background-consolidate the memory store (merge near-duplicates,
    /// resolve contradictions, convert relative dates, prune stale
    /// entries, regenerate the index). Fire-and-forget: the spawned fork
    /// runs on a tokio task so the main loop is never blocked. Only the
    /// main runner wires this; the forked runners leave it None so it never
    /// recursively self-triggers.
    pub fn with_dream(mut self, dream: Arc<crate::agent::auto_dream::DreamRunner>) -> Self {
        self.dream = Some(dream);
        self
    }

    /// Install runtime-flippable memory toggles. The composition root creates
    /// the Arc handles, passes clones here so the drive loop gates on them,
    /// and keeps clones for the host so a toggle command can flip them
    /// mid-session (the change lands on the next gate check, no restart).
    pub fn with_toggles(
        mut self,
        auto_memory: Arc<std::sync::atomic::AtomicBool>,
        auto_dream: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        self.auto_memory = auto_memory;
        self.auto_dream = auto_dream;
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

    /// Fire the background memory subsystems at query-loop end (FinalOutput):
    /// the extractor (conversation to memory) and the dream (consolidation).
    /// Both fire-and-forget off the hot path; a failure logs + never fails
    /// the run. Only the main runner wires these (None on forked runners), so
    /// neither recursively self-triggers.
    pub(crate) async fn fire_background_at_finaloutput(&self, session: SessionId) {
        use std::sync::atomic::Ordering;
        if std::env::var("HOUYICODER_REWARD_OFF").is_ok() {
            return;
        }
        // auto_memory gates the extractor (the cheaper, deterministic half).
        // The dream is gated separately by auto_dream so a user who wants
        // extraction but not the consolidation LLM can keep the former.
        if self.auto_memory.load(Ordering::Relaxed)
            && let Some(ext) = self.extractor.as_ref()
        {
            match self.store.replay(session).await {
                Ok(msgs) => ext.extract_memories(msgs),
                Err(e) => tracing::warn!("memory extract replay failed: {e}"),
            }
        }
        if self.auto_dream.load(Ordering::Relaxed)
            && let Some(dream) = self.dream.as_ref()
        {
            // Reward snapshot: lock OL + redundancy briefly, clone out, drop
            // both before the fire-and-forget spawn. The snapshot is owned
            // and moved into the dream task; DreamRunner never holds OL.
            let reward = crate::agent::reward_snapshot::project_reward_snapshot(
                &self.observability,
                &self.redundancy,
            );
            dream.execute_dream(Some(reward), Some(&session.to_string()));
        }
    }

    /// Await in-flight dream tasks (reward-dream or consolidation) until
    /// they finish or the timeout expires. Tests use this instead of
    /// polling dream_count on a sleep loop — the JoinHandle await is
    /// event-driven (the scheduler wakes on task completion), and the
    /// deadline is a safety bound, not a poll interval.
    pub async fn join_dreams(&self, timeout: std::time::Duration) {
        if let Some(dream) = self.dream.as_ref() {
            dream.drain_pending(timeout).await;
        }
    }

    /// Compress the session (auto path, fired by the pre-flight + overflow
    /// handler in the agent loop): fold older events into a summary, persist
    /// a checkpoint manifest, append CompactionBoundary + Summary events
    /// (hash chain maintained), and fire PreCompact/PostCompact hooks.
    /// Delegates to compact_internal so the manual /compact path and the
    /// auto path share one hook-fire + marker-extraction sequence (only the
    /// trigger differs). Returns true when at least one event was Summarized
    /// (progress was made). When false, the manifest is all-Verbatim and
    /// compressing again would not shrink the window — the caller must
    /// fail-closed.
    pub async fn compress(&self, session: SessionId) -> Result<bool, RunError> {
        // Flag for cache-break attribution: compaction rewrites the prefix,
        // so the next provider response will likely show a cache-read drop.
        // The flag is cleared in append_turn_usage after attribution.
        self.cache_compact_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let outcome = self
            .compact_internal(session, crate::agent::hook::CompactTrigger::Auto)
            .await?;
        Ok(outcome.made_progress)
    }

    /// Before-clear marker extraction: scan the whole session for
    /// unsolved-problem and key-decision markers, write them to the auto
    /// scope so key facts survive the /clear drop. Matches the before-compact
    /// extraction but scans every event (clear drops everything, not just a
    /// folded span). Deterministic, no model, best-effort — a write failure
    /// logs and continues; memory persistence never blocks the clear path.
    /// No-op when no memory provider is wired.
    pub async fn before_clear(&self, session: SessionId) -> Result<(), super::RunError> {
        let Some(memory) = &self.memory else {
            return Ok(());
        };
        let events = self.store.replay(session).await?;
        let existing: std::collections::HashSet<String> =
            memory.list_memories().into_iter().map(|s| s.key).collect();
        for entry in super::lifecycle::extract_preclear_markers(&events) {
            if existing.contains(&entry.key) {
                continue;
            }
            if let Err(e) = memory.add(entry) {
                tracing::warn!("before-clear marker write failed: {e}");
            }
        }
        Ok(())
    }

    /// Override the default heuristic summarizer with an LLM-backed one. The
    /// composition root calls this at construction to wire production summaries.
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
    /// when no provider is wired. Capped at 200 entries.
    pub fn format_memory_index(&self) -> Option<String> {
        let memory = self.memory.as_ref()?;
        let summaries = memory.list_memories();
        if summaries.is_empty() {
            return None;
        }
        let lines: String = summaries
            .iter()
            .take(200)
            .map(|s| {
                format!(
                    "- {} [{}/{}]: {}\n",
                    s.key,
                    s.source.as_label(),
                    s.origin.as_label(),
                    s.description
                )
            })
            .collect();
        Some(lines)
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
        CheckpointId, CheckpointManifest, Disposition, EventId, SessionId, TurnEvent,
        TurnEventKind, TurnGroup,
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
        let event = TurnEvent {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
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
        let event = TurnEvent {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
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
        let event = TurnEvent {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
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
        use houyicoder_context::{MemoryEntry, MemorySource};
        use houyicoder_memory::MarkdownMemoryProvider;
        let root =
            std::env::temp_dir().join(format!("mem-index-{}-{}", std::process::id(), line!()));
        drop(std::fs::remove_dir_all(&root));
        std::fs::create_dir_all(&root).expect("mkdir");
        let memory: Arc<dyn houyicoder_api::memory::MemoryProvider> =
            Arc::new(MarkdownMemoryProvider::new(root.clone()));
        memory
            .add(MemoryEntry::new(
                "proj-pref",
                "prefer let chains",
                MemorySource::Project,
            ))
            .unwrap();
        let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let runner = Runner::with_shared_store(
            store,
            Arc::new(crate::provider::test_support::FakeProvider::new(vec![])),
            crate::agent::ToolRegistry::new(),
            crate::agent::runner_config::RunnerConfig::default(),
        )
        .with_memory(memory);
        let idx = runner.format_memory_index();
        assert!(idx.is_some(), "index returned with provider wired");
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
