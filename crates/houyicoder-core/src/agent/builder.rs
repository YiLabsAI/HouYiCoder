//! Configures Runner collaborators and runtime policy.

use std::path::PathBuf;
use std::sync::Arc;

use houyicoder_context::SessionId;
use houyicoder_resilience::resource_breaker::ResourceBreaker;

use crate::agent::SkillHookRegistrar;
use crate::agent::ToolRegistry;
use crate::agent::backbone::WorkspaceProbe;
use crate::agent::conditional_activation::ConditionalSkillActivator;
use crate::agent::context::AssemblyInput;
use crate::agent::hook::CompactTrigger;
use crate::agent::hook::registry::HookRegistry;
use crate::agent::manifest::Summarizer;
use crate::agent::memory::{MemoryRuntime, MutationLog};
use crate::agent::multi_agent::bus_types::BusMessage;
use crate::agent::reducer::ToolOutputReducer;
use crate::agent::skill_reload::SkillReloadGuard;
use crate::agent::tools::{MemoryAddTool, SearchMemoryTool, ShowMemoryTool};
use crate::agent::{RunError, Runner, VerifyGate};
use houyicoder_api::agent_event::AgentEventHandlers;
use houyicoder_api::cache_policy::CachePolicyProvider;
use houyicoder_api::cost_model::CostModelProvider;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::skill::{SkillDescriptor, SkillRegistry, SkillSnapshot};
use houyicoder_api::trust::TrustState;
use houyicoder_context::Disposition;
use houyicoder_context::MemoryOrigin;
use houyicoder_protocol::llm::ProviderError;
use houyicoder_protocol::llm::ToolDef;
use std::sync::RwLock;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

/// The capability-derived inputs one assembly reads: the tool schemas the
/// request declares, the frozen memory index, and the skills the listing
/// announces. Owned so the caller can hold them across the assembler's borrow.
pub(crate) struct AssemblyParts {
    pub tool_defs: Vec<ToolDef>,
    pub memory_index: Option<String>,
    pub skills: Vec<SkillSnapshot>,
}

impl AssemblyParts {
    /// The borrow the assembler reads.
    pub(crate) fn input(&self) -> AssemblyInput<'_> {
        AssemblyInput {
            tool_defs: &self.tool_defs,
            memory_index: self.memory_index.as_deref(),
            skills: &self.skills,
        }
    }
}

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
    pub fn cwd_handle(&self) -> Arc<RwLock<PathBuf>> {
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
    pub fn set_inbox(&mut self, rx: UnboundedReceiver<BusMessage>) {
        *self.inbox.lock().expect("inbox lock") = Some(rx);
    }

    /// Clone the installed event handlers for a staged collaborator.
    pub fn event_handlers(&self) -> AgentEventHandlers {
        self.events.clone()
    }

    /// Install a fully constructed memory runtime. Registers the
    /// save_memory tool sharing the runtime's provider so main-agent saves
    /// and forked-extract saves land under the same lock, plus the two
    /// read-only tools the agent needs to find a memory again and read one
    /// body back before acting on it. Consumes and returns self for chaining
    /// at the composition root.
    pub fn install_memory(mut self, mut runtime: MemoryRuntime) -> Self {
        runtime.set_event_handlers(&self.events);
        if let Some(provider) = runtime.provider().cloned() {
            let recorder = runtime.install_primary_recorder();
            self.tools.register(Arc::new(
                MemoryAddTool::new(provider.clone())
                    .with_origin(MemoryOrigin::MainAgent)
                    .with_recorder(recorder),
            ));
            self.tools.register(Arc::new(
                SearchMemoryTool::new(provider.clone()).with_reranker(runtime.reranker().cloned()),
            ));
            self.tools.register(Arc::new(ShowMemoryTool::new(provider)));
        }
        self.memory = runtime;
        self
    }

    /// Install memory writes and mutation tracking for an extraction run.
    /// The tool is the pinned construction: extractor origin, an auto-root
    /// pin for new keys, and the evidence window every save must quote.
    pub(crate) fn install_extraction_memory(
        mut self,
        provider: Arc<dyn MemoryProvider>,
        recorder: Arc<MutationLog>,
        evidence: Arc<[houyicoder_context::SessionLogEntry]>,
    ) -> Self {
        self.tools.register(Arc::new(MemoryAddTool::new_extraction(
            provider.clone(),
            recorder,
            evidence,
        )));
        self.memory.install_provider(provider);
        self
    }

    /// Read access to the tool registry. The composition root + tests use it
    /// to assert a tool is registered (e.g. save_memory after install_memory).
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Install the hook registry. When set, the runner fires PreToolUse before
    /// each tool execution and PostToolUse / PostToolUseFailure after, then
    /// combines the verdicts to drive flow control (Deny blocks the call,
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
    pub fn with_registrar(mut self, registrar: Arc<SkillHookRegistrar>) -> Self {
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
    pub fn set_trust(&self, state: TrustState) {
        if let Some(r) = self.registrar.as_ref() {
            r.set_trust(state);
        }
    }

    /// Override the cache policy (defaults to the Auto three-breakpoint set).
    /// A provider with no prompt-cache support swaps NoCachePolicy; a
    /// config-driven explicit policy lands when that wires.
    pub fn with_cache_policy(mut self, policy: Arc<dyn CachePolicyProvider>) -> Self {
        self.cache_policy = policy;
        self
    }

    /// Override the per-provider cost model (defaults to the Anthropic-pricing
    /// rates). A multi-provider runtime swaps per active provider so the
    /// economy-driven compaction gate uses the right cache-read/write ratios.
    pub fn with_cost_model(mut self, model: Arc<dyn CostModelProvider>) -> Self {
        self.cost_model = model;
        self
    }

    /// A shared handle to the recall meter, so the composition root can pass
    /// the same Arc to the conversation recall tool (the tool bumps it on a
    /// match in the folded span) and the compaction path (it snapshots +
    /// resets to compute the recall rate). The composition root registers the
    /// tool with this handle so the tool + the runner share one counter.
    pub fn recall_meter(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.recall_meter)
    }

    /// Install a workspace probe for the re-derivable compaction backbone's
    /// derivation watermark. The composition root passes a GitWorkspaceProbe
    /// sharing the cwd handle so worktree switches propagate. Called pre-Arc,
    /// before sharing the runner. None (the default) means the backbone runs the
    /// log-rederivable layer only; the workspace watermark fields are None.
    pub fn set_workspace_probe(&mut self, probe: Arc<dyn WorkspaceProbe>) {
        self.workspace_probe = Some(probe);
    }

    /// Install a tool-output reducer. The isolate stage reduces a large tool
    /// result (strip ansi, head/tail, truncate) before serving it so the
    /// served preview is compact; the raw stays in the CAS. None (the
    /// default) ⇒ no reduction (the raw preview is served). The composition
    /// root wires a HotPathReducer for the built-in tools.
    pub fn with_reducer(mut self, reducer: Arc<dyn ToolOutputReducer>) -> Self {
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
    pub fn with_recall_meter(mut self, meter: Arc<AtomicU32>) -> Self {
        self.recall_meter = meter;
        self
    }

    /// Install the skill registry used to build the
    /// skill-discovery listing attachment. The registry is discovered at
    /// startup; the same Arc is shared with the Skill tool (registered
    /// separately at the composition root) so invocation + listing see one
    /// set. None in tests and the pure-stub path (no listing attached).
    pub fn with_skill_registry(mut self, registry: Arc<dyn SkillRegistry>) -> Self {
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
    pub fn refresh_served_models(&self) -> houyicoder_async::PFut<'_, Result<(), ProviderError>> {
        self.provider.refresh_served_models()
    }

    /// Await running background memory tasks — recall selections first,
    /// then dream passes (reward-dream or consolidation) — until they
    /// finish or the timeout expires. Tests use this instead of polling
    /// dream_count on a sleep loop — the JoinHandle await is event-driven
    /// (the scheduler wakes on task completion), and the deadline is a
    /// safety bound, not a poll interval.
    pub async fn join_dreams(&self, timeout: Duration) {
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
        self.cache_compact_flag.store(true, Ordering::Relaxed);
        let outcome = self.run_compaction(session, CompactTrigger::Auto).await?;
        Ok(outcome.made_progress)
    }

    /// System lifecycle seam for clear: hand the session to the memory
    /// runtime's before-clear boundary, which drops the cached index and
    /// submits the durable log to the background extraction pipeline.
    /// Infallible by contract — preservation never blocks the clear path;
    /// a failed extraction job retries off the durable log later.
    pub async fn before_clear(&self, session: SessionId) {
        self.memory.before_clear(session).await;
    }

    /// Override the default heuristic summarizer with an LLM-backed one. The
    /// composition root installs the production implementation.
    pub fn with_summarizer(mut self, summarizer: Box<dyn Summarizer>) -> Self {
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

    /// Return the session-frozen memory index for the system prompt prefix.
    pub fn format_memory_index(&self) -> Option<String> {
        self.memory.format_index()
    }

    /// The capability-derived inputs one assembly reads. Built in one place so
    /// the turn path and the /context measurement hand the assembler the same
    /// tools, index, and skills. A second construction would let the reported
    /// footprint drift from the one the request pays.
    pub(crate) fn assembly_parts(&self) -> AssemblyParts {
        AssemblyParts {
            tool_defs: self.tools.tool_defs(),
            memory_index: self.format_memory_index(),
            skills: self.announced_skills(),
        }
    }

    /// The skills the discovery listing announces: the model-invocable set
    /// minus the paths-gated skills that are not active this session. One
    /// definition, shared by the listing the request carries and the cost the
    /// /context view reports.
    pub(crate) fn announced_skills(&self) -> Vec<SkillSnapshot> {
        self.skills_snapshot()
            .into_iter()
            .filter(|snapshot| self.skill_announced(&snapshot.descriptor))
            .collect()
    }

    /// True when a skill's listing entry reaches the model. A skill the
    /// registry disables for model invocation never does; a paths-gated skill
    /// announces only while its paths are active. An activation state that was
    /// never set leaves the gate off, so the entry shows rather than
    /// disappearing.
    fn skill_announced(&self, descriptor: &SkillDescriptor) -> bool {
        if descriptor.disable_model_invocation {
            return false;
        }
        let Some(registry) = &self.skill_registry else {
            return false;
        };
        if registry.paths_for(&descriptor.name).is_empty() {
            return true;
        }
        self.conditional
            .as_ref()
            .map(|activation| activation.is_active(&descriptor.name))
            .unwrap_or(true)
    }

    /// Token count of the compact summary text (the replacement for folded
    /// turns). Returns 0 when no compaction has run, when the summary is empty,
    /// and when the latest compaction predates a clear: the context the clear
    /// started carries no folded turn. Used to populate a "Compact buffer"
    /// category in /context.
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
    /// compaction describes the context the session holds, which includes the
    /// case where the latest one predates a clear. Counts folded turn groups +
    /// truncates the summary preview to one line.
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
            .filter(|g| matches!(g.disposition, Disposition::Summarized))
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
mod tests;
