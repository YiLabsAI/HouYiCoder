//! agent::runner — the spine: the single-agent tool-use loop.
//!
//! The loop holds no in-memory message buffer; the durable session log is the
//! buffer, re-projected into model input each turn. Stateless provider (full
//! history each call) keeps replay server-safe, so Retry only vetoes transient
//! provider errors. Tool dispatch is sequential with a partition-by-safety
//! batch for concurrency-safe calls; handoff is terminal. The store is held
//! behind an Arc<dyn SessionLog> so callers sharing the runner can replay
//! events for a live transcript view while a run is in flight.

mod abort;
mod append;
mod approval;
pub mod auto_dream;
mod backbone;
mod builder;
mod cache_liveness;
mod call;
pub mod compact;
mod context;
mod diff;
mod durable_scan;
mod economy;
mod effort;
mod entry;
mod exports;
pub mod extract;
pub mod extractor;
mod fact;
pub mod git_discard;
mod hook;
mod input_queue;
mod lifecycle;
mod manifest;
mod memory_recall;
pub mod model_window;
mod obs_wire;
mod projection;
mod prompt;
mod recover;
mod reducer;
mod redundancy;
mod resolve;
mod retention;
pub(crate) mod reward_snapshot;
mod skill_body;
mod skill_listing;
mod skill_slash;
mod status;
mod step;
mod synthetic;
mod thinking;
mod tool;
mod tools;
mod truncation;
mod turn_group;
mod verify;
pub mod worktree_controller;
pub mod worktree_session;

pub mod multi_agent;

pub use effort::{
    EffortResolver, apply_effort_settings, effort_default_for, resolve_applied_effort,
};
pub use exports::*;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use houyicoder_api::live::LiveSink;
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_protocol::llm::Usage;
use houyicoder_protocol::llm::{CompletionResponse, EffortLevel};
use houyicoder_resilience::resource_breaker::ResourceBreaker;

use status::SharedUsage;

use append::new_event;
use call::accumulate_usage;
use synthetic::SyntheticToolOutcome;

pub mod runner_config;
use runner_config::RunnerConfig;

mod result;
pub use result::{RunError, RunOutcome, RunResult};

/// The single-agent tool-use loop. Owns a SessionStore (lossless event log),
/// a ModelProvider (LLM seam), and a ToolRegistry. run drives the loop to
/// completion or interruption; resume continues after a caller resolves
/// approval requests. The store is held behind an Arc so callers that share
/// the runner (a TUI host) can clone the handle and replay events for a live
/// transcript view while a run is in flight.
pub struct Runner {
    store: Arc<dyn houyicoder_api::session::SessionLog>,
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
    config: RunnerConfig,
    /// The active model id, mutable at runtime (the /model pane select).
    /// Seeded from config.model at construction; set_model swaps it so the
    /// next completion request serves the new id without rebuilding the provider
    /// (the provider is stateless about the model — the id is per-request).
    active_model: Arc<std::sync::RwLock<String>>,
    /// The active effort pick for the session, or None to follow the
    /// resolution chain (catalog[id].effort → model.effort_level →
    /// effort_default_for(model) → None). Seeded None at construction; the
    /// /model pane's effort toggle sets it. set_effort swaps it so the next
    /// completion request carries the new level without rebuilding anything.
    active_effort: Arc<std::sync::RwLock<Option<EffortLevel>>>,
    /// Optional catalog-backed effort resolver (the composition root wires an
    /// impl backed by the loaded ModelSection). None on the stub path; the
    /// chain then stops at the in-session pick + built-in default.
    effort_resolver: Option<Arc<dyn EffortResolver>>,
    /// Composes the served view per turn: the assembled system prompt + the
    /// projected message history. The loop calls build() each turn so the
    /// served view and the /context breakdown share one source.
    context_builder: ContextBuilder,
    /// Optional live-event sink the runner notifies while a turn streams. The
    /// host (a TUI) installs a closure that adapts LiveEvent into its own render
    /// path; the runner knows nothing about the host's types. None in tests and
    /// the pure-stub path ⇒ streaming still works, just with no live preview.
    live: Option<LiveSink>,
    /// Bus inbox receiver for a spawned child: the parent publishes
    /// BusMessage::Inbox texts and the drive loop drains them at each turn
    /// boundary, appending each as a user message before the next model
    /// call. None for the top-level runner (no bus steering).
    inbox: std::sync::Mutex<
        Option<
            tokio::sync::mpsc::UnboundedReceiver<crate::agent::multi_agent::bus_types::BusMessage>,
        >,
    >,
    /// Startup warnings (bad settings fields, network policy typos) queued
    /// for the host to surface as initial transcript system lines. The
    /// composition root collects these during build; the host drains them at
    /// pair time + pushes them synchronously so they land before any run
    /// output (no async-sink race with test assertions).
    startup_warnings: std::sync::Mutex<Vec<String>>,
    /// Optional aggregate resource breaker shared with the sandbox. Attached at
    /// the composition root so status reads the same breaker the sandbox
    /// enforces against; enforcement stays in the sandbox (the runner never
    /// calls try_acquire), this field is read-only for status reporting. None
    /// when no breaker is wired (tests, the stub path).
    breaker: Option<Arc<ResourceBreaker>>,
    /// Shared cumulative usage accumulator across runs on this runner. The drive
    /// loop writes per-turn; status_snapshot reads it for /context + /compact.
    /// Reset on a new session by the host calling reset_usage.
    usage: SharedUsage,
    /// Shared observability log (thin aggregator: tool stats / cost / per-turn
    /// delta); tracks the usage field's shared-mutex shape.
    observability: obs_wire::SharedObservability,
    /// The active run's cancellation token, set by run()/resume() and dropped
    /// when the run returns. abort() cancels it so the in-flight stream loop
    /// flushes partial text + reconciles orphan tool results. Guarded by a
    /// std Mutex (held only for the brief set/read at run start + abort).
    cancel: std::sync::Mutex<Option<CancellationToken>>,
    aborted: std::sync::atomic::AtomicBool,
    paused: std::sync::atomic::AtomicBool,
    /// The active turn's cancellation token, set at each turn start +
    /// cleared when the turn ends. A per-turn abort (the viewed-child Esc
    /// path) cancels it so the in-flight model fetch for this turn aborts
    /// without killing the run: the drive loop appends an interrupt
    /// marker + starts the next turn with a fresh token. Distinct from
    /// the cancel field (the lifecycle token, terminal). Guarded by a std
    /// Mutex.
    turn_cancel: std::sync::Mutex<Option<CancellationToken>>,
    /// Optional post-run verification gate. When set, after a run reaches
    /// FinalOutput the runner calls verify before returning. A failed verify
    /// surfaces RunOutcome::VerifyFailed instead of FinalOutput so the caller
    /// can re-prompt the model. None means no gate, FinalOutput passes through.
    verify_gate: Option<Arc<dyn VerifyGate>>,
    /// The undo stack + snapshot store for recoverable destructive ops.
    /// Shared with the BashTool (which pushes); undo_last pops + restores.
    undo_stack: Option<Arc<std::sync::Mutex<crate::snapshot::UndoStack>>>,
    snapshot_store: Option<Arc<crate::snapshot::SnapshotStore>>,
    /// Snapshot retention TTL in seconds; prune_snapshots drops older entries.
    snapshot_ttl_secs: u64,
    /// Snapshot store size cap in bytes; prune_snapshots enforces it.
    snapshot_size_cap_bytes: u64,
    /// The summarizer for the Compress stage. Defaults to HeuristicSummarizer
    /// (no LLM dependency, used in tests). The composition root overrides with
    /// LlmSummarizer for production so compress produces real summaries.
    summarizer: Box<dyn manifest::Summarizer>,
    /// Optional persistent memory provider. When wired, the turn-entry step
    /// recalls relevant entries per user query and appends a durable
    /// memory-recall attachment the projection merges into the turn's user
    /// message — so the system prompt stays byte-frozen across turns. The
    /// surfaced de-dup set is scanned from the projected transcript, so
    /// compaction (which folds old memory-recall events out) is the natural
    /// reset point. None in tests and the pure-stub path (no cross-session
    /// memory).
    memory: Option<Arc<dyn houyicoder_api::memory::MemoryProvider>>,
    /// Optional skill registry. When wired, the turn-entry step appends a
    /// skill-discovery listing attachment (descriptions only — the Skill
    /// tool loads bodies on demand). None in tests and the pure-stub path
    /// (no skill discovery).
    skill_registry: Option<Arc<dyn houyicoder_api::skill::SkillRegistry>>,
    /// Optional hook registry. When wired, the runner fires PreToolUse
    /// before each tool execution and PostToolUse / PostToolUseFailure
    /// after, arbitrating verdicts (Deny blocks, Feedback surfaces a
    /// self-correction signal, Observe logs, Trigger fires downstream, Allow
    /// proceeds). None means no hooks fire at runtime. See hook_pipeline.rs.
    hooks: Option<Arc<crate::agent::hook::registry::HookRegistry>>,
    /// Prompt-cache breakpoint policy. Decides where to carve a stable prefix
    /// for prompt-cache reuse (the wire kinds live in the wire crate;
    /// the provider lowers each kind to its own format). Defaults to the Auto
    /// three-breakpoint set; a provider with no cache support swaps NoCachePolicy.
    cache_policy: Arc<dyn houyicoder_api::cache_policy::CachePolicyProvider>,
    /// Per-provider token-cost model. Drives the economy-driven compaction
    /// gate (projected cache savings vs rewrite + summarizer cost). Defaults
    /// to the Anthropic-pricing rates; a multi-provider runtime swaps per
    /// active provider.
    cost_model: Arc<dyn houyicoder_api::cost_model::CostModelProvider>,
    /// Recall meter: counts conversation_search matches that landed in the
    /// folded (Summarized) span since the last compaction. The conversation
    /// recall tool bumps it; compact_internal snapshots + resets it to
    /// compute a recall rate (recalls / folded count) for the compaction
    /// report. Shared (Arc) so the tool + the compaction path share one
    /// counter across the session. A fresh meter starts at 0 (no recalls).
    recall_meter: Arc<std::sync::atomic::AtomicU32>,
    /// Optional workspace probe for the re-derivable compaction backbone's
    /// derivation watermark (git rev + dirty-tree hash). None in tests + the
    /// pure-stub path; the composition root wires a GitWorkspaceProbe sharing
    /// the cwd handle so worktree switches propagate. None ⇒ the backbone
    /// runs the log-rederivable layer only (the watermark fields are None).
    workspace_probe: Option<Arc<dyn backbone::WorkspaceProbe>>,
    /// Auto-compact suppression level (a CompactSuppress as u8). Set by a
    /// deterministic compact failure + read by the pre-flight economy gate;
    /// manual /compact bypasses it. The turn-start self-heal clears Turn.
    compact_suppress: std::sync::atomic::AtomicU8,
    /// Cached-prefix liveness + per-block stable retention decisions; shared
    /// with the ContextBuilder (see cache_liveness).
    cached_prefix: std::sync::Arc<cache_liveness::CachedPrefixState>,
    /// Consecutive transient (Other-class) auto-compact failures. A fatal
    /// cause goes Sticky on the first failure; a transient cause self-heals
    /// each turn and would retry every turn, so a streak promotes it to Sticky
    /// after 3, stopping a persistently-failing transient cause from hammering
    /// a doomed compact each turn. Reset on a successful compact + on a
    /// context-budget change.
    compact_consecutive_failures: std::sync::atomic::AtomicU32,
    /// Previous turn's cache_read for break detection (None before first turn).
    cache_prev_read: std::sync::Mutex<Option<u64>>,
    /// Flag: compaction ran since the previous provider response.
    cache_compact_flag: std::sync::atomic::AtomicBool,
    /// Flag: model switched since the previous provider response.
    cache_model_switch_flag: std::sync::atomic::AtomicBool,
    /// Optional tool-output reducer; the isolate stage reduces a large tool
    /// result before serving it.
    reducer: Option<Arc<dyn reducer::ToolOutputReducer>>,
    /// Optional memory extractor that fires at query-loop end (FinalOutput,
    /// no tool calls) to background-extract memories from the conversation.
    /// Fire-and-forget: the spawned fork runs on a tokio task so the main
    /// loop is never blocked. Only the main runner wires this (the forked
    /// extraction runner does not), so it never recursively self-triggers.
    /// None in tests and the pure-stub path.
    extractor: Option<Arc<crate::agent::extractor::MemoryExtractor>>,
    /// Consolidation dream firing at query-loop end (fire-and-forget, off the
    /// hot path). None on forked runners (no self-trigger) + tests.
    dream: Option<Arc<crate::agent::auto_dream::DreamRunner>>,
    /// Runtime-flippable memory feature switches shared with the host. The
    /// host flips them via a command and the change takes effect on the next
    /// gate check (no runner restart). auto_memory gates turn-entry recall
    /// injection + the background extractor; auto_dream gates the
    /// consolidation dream. Default on; constructed from the persisted
    /// settings file at the composition root. AtomicBool so a host thread
    /// flips while the drive loop reads — no lock, no contention on the hot
    /// path.
    auto_memory: Arc<std::sync::atomic::AtomicBool>,
    auto_dream: Arc<std::sync::atomic::AtomicBool>,
    /// The mid-turn injection queue: user messages the host submitted while a
    /// run is in flight. The drive loop drains it at each turn boundary (after
    /// a tool resolves, before the next model call) and appends each as a user
    /// message so the model sees the interjection on its next call + responds +
    /// resumes the in-flight task — the turn-boundary injection (a queued
    /// message fed into the same turn's next request, finer than the
    /// run-boundary queue the host also keeps for inputs that land after a run
    /// ends). Single source of truth: the queue lives here on the host (where
    /// the loop polls it), never on the guest — guests do not share the host
    /// heap. The frontend keeps a derived copy (its run-boundary queue) +
    /// reconciles it from the user-message stream (no second source of truth,
    /// no ack channel). std Mutex: drain is non-blocking, no await under the
    /// lock.
    queued_input: std::sync::Mutex<std::collections::VecDeque<String>>,
    /// Lower-priority notifications (an async child completed). Drained at a
    /// turn boundary only after queued_input is empty, so a user interjection
    /// always lands before a notification that queued at the same instant —
    /// notifications never starve user input. std Mutex: drain is non-blocking,
    /// no await under the lock.
    queued_notifications: std::sync::Mutex<std::collections::VecDeque<(String, String)>>,
    /// Texts the drive loop drained from queued_input this run. The host
    /// reads + clears it at run end so it can tell the frontend which queued
    /// messages were injected (the frontend removes them from its copy).
    /// Per-run: take_consumed_input drains, so a fresh run starts empty.
    consumed_input: std::sync::Mutex<Vec<String>>,
    /// Redundant-call detector — a harness self-evolution observer,
    /// independent of the user hook registry (which early-returns when no
    /// hooks are configured). check_batch runs before arbitrate_pre_tool_use
    /// (resolve_turn); record runs next to fire_post_tool_use. Held behind a
    /// std Mutex, brief pure compute, no await in the lock.
    redundancy: std::sync::Mutex<redundancy::RedundancyTracker>,
    /// Agent types deny rules block, threaded into every ToolCtx so the
    /// agent tool can tell a denial from an unknown type. Set once at the
    /// composition root (where permission rules live); empty by default.
    denied_agents: std::sync::Arc<std::collections::HashSet<String>>,
    /// Spawn port + identity the dispatch threads into the agent tool's ctx.
    spawn_handle: Option<std::sync::Arc<dyn houyicoder_api::spawn::SpawnHandle>>,
    agent_identity: houyicoder_api::spawn::AgentIdentity,
}

impl Runner {
    /// Construct a runner over the given store, provider, tools, and config.
    /// The store is wrapped in an Arc internally; use store() to get a shared
    /// handle back for side-channel replay reads. No breaker is attached;
    /// /sandbox + /status report None for breaker state. Use with_breaker at
    /// the composition root to share the sandbox's breaker for status.
    /// Construct a runner owning its store. Tests use this; the composition
    /// root uses with_shared_store (same body) so a host can replay while the
    /// runner appends. Delegates so the two entry points share one body.
    pub fn new(
        store: Arc<dyn houyicoder_api::session::SessionLog>,
        provider: Arc<dyn ModelProvider>,
        tools: ToolRegistry,
        config: RunnerConfig,
    ) -> Self {
        Self::with_shared_store(store, provider, tools, config)
    }

    /// A shared handle to the session store. Clone it to replay events from a
    /// side channel (the TUI host does this after each run to refresh the
    /// transcript view from real TurnEvents).
    pub fn store(&self) -> Arc<dyn houyicoder_api::session::SessionLog> {
        self.store.clone()
    }

    /// The skill registry, when wired. The approval path reads it to detect
    /// when a Bash command runs a skill-directory script so the prompt can
    /// show what would execute. None on the pure-stub path (no discovery).
    pub fn skill_registry(&self) -> Option<&Arc<dyn houyicoder_api::skill::SkillRegistry>> {
        self.skill_registry.as_ref()
    }

    /// The dream's cross-session scan root, or None when in-memory.
    pub fn dream_session_log_root(&self) -> Option<&std::path::Path> {
        let dream = self.dream.as_ref();
        dream.and_then(|d| d.session_log_root.as_deref())
    }

    /// The active model id (the /model pane select). The runner reads this per
    /// request; set_model swaps it so the next completion serves the new id
    /// without rebuilding the provider (the id is per-request).
    pub fn active_model(&self) -> String {
        self.active_model
            .read()
            .map(|m| m.clone())
            .unwrap_or_default()
    }

    /// The active effort pick, or None to follow the resolution chain
    /// (catalog → model.effort_level → per-model default → None). The runner
    /// reads this per request when building ModelSettings; set_effort swaps
    /// it so the next completion carries the new level.
    pub fn active_effort(&self) -> Option<EffortLevel> {
        self.active_effort.read().map(|e| *e).ok().flatten()
    }

    /// Set the active effort for the session (the /model pane effort toggle).
    /// None means follow the resolution chain (auto); a level is a sticky
    /// per-session pick. The next completion request carries the new value.
    pub fn set_effort(&self, effort: Option<EffortLevel>) {
        if let Ok(mut e) = self.active_effort.write() {
            *e = effort;
        }
    }

    /// Resolve the effort level the next completion request should carry,
    /// following the chain: active pick → catalog (via the wired resolver) →
    /// per-model default → None. Short-circuits to None for a model the dialect
    /// probe does not recognize (I8). The composition root wires the resolver
    /// from the loaded ModelSection; None means the stub path stops at the
    /// active pick + built-in default.
    pub fn resolve_applied_effort(&self) -> Option<EffortLevel> {
        let model = self.active_model();
        resolve_applied_effort(
            &model,
            self.active_effort(),
            self.effort_resolver.as_deref(),
        )
    }

    /// Wire a catalog-backed effort resolver (the composition root injects an
    /// impl backed by the loaded ModelSection). Builder-style so the runner
    /// assembles in one statement.
    pub fn with_effort_resolver(mut self, resolver: Arc<dyn EffortResolver>) -> Self {
        self.effort_resolver = Some(resolver);
        self
    }

    /// Set the denied-agent set the agent tool reads at resolve time.
    pub fn with_denied_agents(mut self, denied: Arc<std::collections::HashSet<String>>) -> Self {
        self.denied_agents = denied;
        self
    }

    /// Resolve the output-token cap the next request carries, same-source for
    /// the pre-flight reserve and the request body (no overflow when the two
    /// disagree). A catalog override (ModelEntry.max_output_tokens) wins over
    /// the construction-time config value; otherwise the config value stands
    /// (the family default resolved at the composition root).
    pub fn resolve_max_output_tokens(&self) -> u32 {
        let model = self.active_model();
        let resolved = self
            .effort_resolver
            .as_deref()
            .and_then(|r| r.catalog_max_output_tokens(&model))
            .unwrap_or(self.config.max_output_tokens);
        // The provider's declared cap is its own real limit; take the min so
        // a provider reporting a smaller cap (tests, a constrained gateway)
        // is respected — the catalog default is a fallback for unknown
        // families, not a floor that overstates the provider's actual room.
        let provider_cap = self.provider.capabilities().max_output_tokens;
        resolved.min(provider_cap)
    }

    /// Wiring probe: true when the composition root wired an LlmSummarizer
    /// (real summaries) rather than the default HeuristicSummarizer
    /// placeholder. Lets a composition-root test assert the production
    /// runner uses a real summarizer without exposing the trait object.
    pub fn summarizer_is_llm(&self) -> bool {
        self.summarizer
            .as_any()
            .downcast_ref::<lifecycle::LlmSummarizer>()
            .is_some()
    }

    /// Switch the active model id (the /model pane select). The provider is
    /// stateless about the model — it serves whatever id the request carries —
    /// so this is a cheap swap, not a provider rebuild.
    pub fn set_model(&self, model: String) {
        if let Ok(mut m) = self.active_model.write() {
            *m = model;
        }
        // A model switch may change the resolved context window (a larger
        // window lifts a sticky suppress that a fatal compact set under the
        // old, smaller window).
        self.clear_sticky_compact_suppress();
        // A model switch changes the provider-facing prefix, so the cached
        // prefix generation + the last observed input tokens are stale.
        self.cached_prefix.invalidate();
        if let Ok(mut ol) = self.observability.lock() {
            ol.clear_last_turn_delta();
        }
        // Flag for cache-break attribution: a model switch likely breaks the
        // prompt cache (different model id on the provider side).
        self.cache_model_switch_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// The loop body. turn is the model-call count before this invocation
    /// (0 for run, prior count for resume) so max_turns is cumulative.
    async fn drive_loop(
        &self,
        session: SessionId,
        turn: u32,
        usage: Usage,
        token: &CancellationToken,
    ) -> Result<RunResult, RunError> {
        // Finalize the injection buffer at the loop exit (single point; every
        // caller covered). Pre-loop returns in resume skip this (low severity).
        let result = self.drive_loop_inner(session, turn, usage, token).await;
        self.finalize_input_buffer(&result);
        result
    }

    async fn drive_loop_inner(
        &self,
        session: SessionId,
        mut turn: u32,
        mut usage: Usage,
        token: &CancellationToken,
    ) -> Result<RunResult, RunError> {
        let mut next_step = NextStep::RunAgain;
        loop {
            match next_step {
                NextStep::RunAgain => {
                    turn += 1;
                    if turn > self.config.max_turns {
                        // Graceful max-turns result (not a crash): the run
                        // is resumable and carries the turns + usage so
                        // the caller can surface cost statistics.
                        return Ok(RunResult {
                            outcome: RunOutcome::MaxTurnsReached { turns: turn - 1 },
                            turns: turn - 1,
                            usage,
                        });
                    }
                    // Mid-turn interjection: drain the host-submitted queue,
                    // the bus inbox, + lower-priority completion notifications,
                    // appending each as a user message before the next model
                    // call. Extracted so the drive loop body stays readable.
                    self.drain_turn_boundary(session).await?;
                    // Arm the per-turn abort token so an external cancel_turn
                    // (the viewed-child Esc path) can abort this turn's model
                    // fetch without killing the run. Cleared after the call.
                    let turn_token = CancellationToken::new();
                    if let Ok(mut guard) = self.turn_cancel.lock() {
                        *guard = Some(turn_token.clone());
                    }
                    let response = self
                        .model_call_stream(session, turn, self.config.max_turns, token, &turn_token)
                        .await?;
                    if let Ok(mut guard) = self.turn_cancel.lock() {
                        *guard = None;
                    }
                    let response = match response {
                        Some(r) => r,
                        None if token.is_cancelled() => {
                            // Lifecycle abort: reconcile orphan results so the
                            // session log stays lossless (no ToolCall without a
                            // ToolResult) then surface the interruption. The
                            // server buffer is cleared at the run/resume exit
                            // (finalize_input_buffer) — Interrupted is terminal.
                            self.reconcile_tool_results(session).await?;
                            return Ok(RunResult {
                                outcome: RunOutcome::Interrupted("interrupted by user".to_string()),
                                turns: turn,
                                usage,
                            });
                        }
                        None => {
                            // Per-turn abort: the viewed-child Esc cancelled
                            // the turn token (not the lifecycle). Drop a
                            // durable marker so the transcript shows the
                            // interrupt + start the next turn with a fresh
                            // token. Non-terminal: the run continues.
                            self.append_turn_interrupted(session).await?;
                            next_step = NextStep::RunAgain;
                            continue;
                        }
                    };
                    accumulate_usage(&mut usage, &response.usage);
                    // Fold into the shared cross-run accumulator so
                    // status_snapshot reports cumulative usage + the last
                    // input_tokens (window footprint) for /context + /compact;
                    // local usage returns per run, the shared one spans the session.
                    if let Ok(mut acc) = self.usage.lock() {
                        acc.record(&response.usage);
                    }
                    next_step = self.resolve_turn(session, &response, token).await?;
                    if matches!(next_step, NextStep::RunAgain) {
                        append::emit_turn_progress(self.live.as_ref(), &response, turn, &usage);
                    }
                }
                NextStep::FinalOutput(text) => {
                    // If a verify gate is installed, run it before
                    // surfacing FinalOutput. A failed verify becomes
                    // RunOutcome::VerifyFailed so the caller can
                    // re-prompt the model to fix its own work. No gate
                    // means FinalOutput passes through unchanged.
                    if let Some(gate) = self.verify_gate.as_ref()
                        && let Err(failure) = gate.verify(session, &*self.store).await
                    {
                        return Ok(RunResult {
                            outcome: RunOutcome::VerifyFailed(failure),
                            turns: turn,
                            usage,
                        });
                    }
                    // Background memory at query-loop end: extractor + dream,
                    // both fire-and-forget; never fails the run.
                    self.fire_background_at_finaloutput(session).await;
                    return Ok(RunResult {
                        outcome: RunOutcome::FinalOutput(text),
                        turns: turn,
                        usage,
                    });
                }
                NextStep::Handoff(agent) => {
                    return Ok(RunResult {
                        outcome: RunOutcome::Handoff(agent),
                        turns: turn,
                        usage,
                    });
                }
                NextStep::Interruption(approvals) => {
                    self.mark_paused();
                    return Ok(RunResult {
                        outcome: RunOutcome::Interruption(approvals),
                        turns: turn,
                        usage,
                    });
                }
            }
        }
    }

    /// Drain the turn-boundary injection buffer: host-submitted user
    /// interjections, bus-inbox steering texts, and lower-priority completion
    /// notifications. Appends each as a user message the next model call sees,
    /// then re-runs memory recall + skill listing for the injected query.
    /// Notifications defer to user input — drained only when the user queue is
    /// empty — so a notification never jumps ahead of a pending user message.
    async fn drain_turn_boundary(&self, session: SessionId) -> Result<(), RunError> {
        let pending_user: Vec<String> = {
            let mut q = self.queued_input.lock().expect("queued_input lock");
            q.drain(..).collect()
        };
        let inbox_pending = self.drain_inbox();
        // Notifications are lower priority than a user interjection: drain
        // them only when no user input is pending, so a notification that
        // queued the same instant as a user message never lands first. A
        // pending notification waits for the next idle boundary.
        let pending_notifications: Vec<(String, String)> = if pending_user.is_empty() {
            let mut q = self
                .queued_notifications
                .lock()
                .expect("queued_notifications lock");
            q.drain(..).collect()
        } else {
            Vec::new()
        };
        let had_pending = !pending_user.is_empty()
            || !inbox_pending.is_empty()
            || !pending_notifications.is_empty();
        if !pending_user.is_empty() {
            self.consumed_input
                .lock()
                .expect("consumed_input lock")
                .extend(pending_user.iter().cloned());
        }
        for msg in pending_user {
            self.append_mid_turn_input(session, msg).await?;
        }
        for msg in inbox_pending {
            self.append_mid_turn_input(session, msg).await?;
        }
        // A child-completion notification is a distinct boundary from a user
        // interjection: it records which child finished (NotificationInjected,
        // not MidTurnInput) so the durable log + the projection distinguish
        // "a background subagent reported a result" from "the user sent a
        // message" -- the model sees a child-completion framing, not a
        // continue-the-task note.
        for (child_id, text) in pending_notifications {
            self.append_notification(session, child_id, text).await?;
        }
        // Recall memory for the injected query (the latest user input), so a
        // mid-turn "now help with X" gets X's memories rather than riding the
        // original turn's recall. No-op when no memory provider is wired.
        if had_pending {
            self.inject_memory_recall(session).await?;
            // A drained mid-turn input may follow a compaction that folded
            // the listing + invoked bodies out; re-announce both so the
            // model keeps skill discovery + the directives for the rest of
            // the run.
            self.inject_skill_listing_and_body(session).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod abort_tool_tests;
#[cfg(test)]
mod ask_question_tests;
#[cfg(test)]
mod budget_pressure_gate_tests;
#[cfg(test)]
mod denied_agents_tests;
mod outcome_counts;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tool_duration_tests;
#[cfg(test)]
mod tool_result_baseline_tests;
#[cfg(test)]
mod turn_abort_tests;
#[cfg(test)]
mod turn_usage_emit_tests;
