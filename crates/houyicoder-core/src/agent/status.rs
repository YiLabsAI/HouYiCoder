//! agent::status — the read-only snapshot a host reads to render the
//! session-health commands (/context, /status, /sandbox). The host asks the
//! runner for what only the engine knows (breaker state + cumulative usage +
//! context window); it overlays the mode and session id it owns itself. This
//! keeps the dependency direction honest: Presentation -> core -> resilience,
//! the host never reaches past the engine into the resilience layer to read
//! breaker state directly.
//!
//! The snapshot is cheap to build: one breaker lock, one accumulator lock,
//! one measurement lock, one provider capability call. No await, no channel.
//! The host may call it on every command dispatch without throttling.

pub(crate) mod obs_wire;
pub(crate) mod redundancy;

use std::sync::{Arc, Mutex};

use houyicoder_protocol::llm::{Usage, UsageProvenance};
use houyicoder_resilience::BreakerState;

use crate::agent::ContextMeasurement;
use crate::agent::RunError;
use crate::agent::Runner;
use crate::agent::append::new_event;
use crate::agent::hook::registry::HookEntry;
use crate::agent::memory::MemoryGateState;
use crate::agent::model_window::resolve_capabilities;
use crate::observability::MetricsView;
use crate::observability::ModelUsage;
use crate::observability::evolution::RedundantCall;
use crate::snapshot::SnapshotStore;
use crate::snapshot::UndoOutcome;
use crate::snapshot::UndoStack;
use houyicoder_api::session::TrajectoryPages;
use houyicoder_api::session::TrajectoryPosition;
use houyicoder_api::session::recorded_turn_usage;
use houyicoder_api::skill::SkillSnapshot;
use houyicoder_context::{SessionEvent, SessionId, SessionLogEntry};
use std::mem::take;
use std::time::Duration;

/// A point-in-time view of the runner's live state for the host to render.
/// Carries only engine-owned state; the mode and session id live in the host
/// (the permission gate and the session handle), so the host composes them
/// around this snapshot rather than the runner echoing them back.
///
/// The breaker fields are deliberately layering-neutral: a state label, a
/// pre-rendered reason string, and a duration. The host renders these as
/// plain strings + a number — it never names a resilience enum, so the
/// dependency stays Presentation -> core -> resilience (the engine matches
/// the resilience type and renders it; the host only prints).
#[derive(Debug, Clone)]
pub struct StatusSnapshot {
    /// The model id the runner sends in CompletionRequest.model.
    pub model: String,
    /// The breaker state as a render label ("Closed" / "Open" / "HalfOpen");
    /// None when the runner has no breaker (tests, the stub path).
    pub breaker_state: Option<&'static str>,
    /// The last trip reason, pre-rendered to a human string, when the breaker
    /// is or was Open. None when the breaker never tripped or none is attached.
    pub breaker_reason: Option<String>,
    /// Remaining cool-down when the breaker is Open and the cool-down has not
    /// elapsed; None otherwise. The host renders the secs; it does not need to
    /// know about the resilience type behind it.
    pub breaker_cool_down: Option<Duration>,
    /// Cumulative provider-reported usage across the whole session, including
    /// the turns a prior runner drove before a reopen, which the accumulator
    /// takes from the log at construction. Inclusive totals plus the
    /// non-overlapping breakdown; see the protocol layer's Usage doc.
    pub cumulative_usage: Usage,
    /// The input_tokens of the last recorded call: the provider's own count
    /// for it, whether this runner made that call or a reopen read it from the
    /// log. Kept as the billing fact; occupancy is not derived from it.
    pub last_input_tokens: u32,
    /// The token count of the latest assembled context, from the same
    /// measurement the context pane renders. None until a turn has assembled
    /// one. This is the occupancy figure: the provider's reported input is a
    /// billing fact and must not stand in for what the request is made of.
    pub context_used_tokens: Option<u32>,
    /// The resolved context window for the active model — the value
    /// resolve_capabilities returns (a learned enforced limit, the catalog,
    /// or the provider caps fallback). Surfaced so the host can render
    /// last_input / window (XX%) without importing provider types. Reads
    /// the live active_model so a /model switch surfaces immediately.
    pub context_window: u32,
    /// Total tool executions across the session (success + error). Surfaces
    /// how chatty the agent is.
    pub tool_calls: u32,
    /// Tool executions that returned a value (not an error payload).
    pub tool_success: u32,
    /// Tool executions that errored (an {"error": ..} payload).
    pub tool_errors: u32,
}

/// Cumulative token usage across turns, shared between the drive loop (writer)
/// and a status snapshot (reader) via an Arc<Mutex>. Tracks the running sum
/// and the last response's input_tokens so /context can show both the
/// cumulative tally and the current window footprint in one read.
#[derive(Debug)]
pub struct UsageAccumulator {
    cumulative: Usage,
    last_input_tokens: u32,
    /// Total tool executions this session (success + error). Counted in
    /// resolve_turn after each tool runs; surfaced in /context so the user
    /// sees how chatty the agent is.
    tool_calls: u32,
    /// Tool executions that returned a value (not an {"error": ..} payload).
    tool_success: u32,
    /// Tool executions that errored (the drive loop wraps ToolError into an
    /// {"error": ..} payload so the model sees it; counted here for /context).
    tool_errors: u32,
}

impl UsageAccumulator {
    /// Fold one response's usage into the cumulative tally and remember its
    /// input_tokens as the current window footprint. Called by the drive loop
    /// after each model call.
    pub fn record(&mut self, turn: &Usage) {
        self.cumulative.input_tokens += turn.input_tokens;
        self.cumulative.output_tokens += turn.output_tokens;
        self.cumulative.total_tokens += turn.total_tokens;
        self.cumulative.non_cached_input_tokens += turn.non_cached_input_tokens;
        self.cumulative.cache_read_input_tokens += turn.cache_read_input_tokens;
        self.cumulative.cache_write_input_tokens += turn.cache_write_input_tokens;
        self.cumulative.reasoning_tokens += turn.reasoning_tokens;
        self.last_input_tokens = turn.input_tokens;
        // The cumulative account is complete only when every folded turn was:
        // an unreported part hides inside a total that otherwise reads as
        // reported. The all-reported seed makes the first turn decide.
        self.cumulative.provenance = self.cumulative.provenance.merge(turn.provenance);
    }

    /// Continue the tally from the turns a session's log already holds, so a
    /// runner that reopened the session reports that session's totals rather
    /// than only the turns since it reopened. The cumulative figures and the
    /// last reported input are seeded, with each turn's completeness folded
    /// the same way record folds a live one.
    ///
    /// The tool counters are not seeded. The log's result payloads are
    /// classified by the session reader's rule and the live loop's by the
    /// observability layer's, so a seeded count would answer the same
    /// question with a second rule and drift from the live one. They stay
    /// per-run; the usage figures above are per-session.
    pub fn seed_from_events(&mut self, events: &[SessionLogEntry]) {
        for entry in events {
            let Some((_, turn)) = recorded_turn_usage(&entry.event) else {
                continue;
            };
            self.cumulative = self.cumulative.saturating_add(&turn);
            self.last_input_tokens = turn.input_tokens;
        }
    }

    /// Fold one tool execution's outcome into the session tally. ok = the
    /// tool returned a value (not an {"error": ..} payload). Called by
    /// resolve_turn after each tool runs.
    pub fn record_tool(&mut self, ok: bool) {
        self.tool_calls += 1;
        if ok {
            self.tool_success += 1;
        } else {
            self.tool_errors += 1;
        }
    }

    /// Fold a batch of tool outcomes into the session tally at once. Used by
    /// resolve_turn which counts success/error across a partition batch then
    /// records under a single lock (the loop holds an await, so the lock
    /// cannot span it).
    pub fn record_tool_batch(&mut self, calls: u32, success: u32, errors: u32) {
        self.tool_calls += calls;
        self.tool_success += success;
        self.tool_errors += errors;
    }

    /// The cumulative tally across all recorded turns.
    pub fn cumulative(&self) -> Usage {
        self.cumulative.clone()
    }

    /// The last response's input_tokens: the provider's own count for the last
    /// call. This is a billing fact, not the occupancy figure — the status bar
    /// and the context pane both read the assembled-context measurement
    /// instead. The observability log's context_pct tracks this provider count
    /// per turn, so the two legitimately differ and neither should be made to
    /// follow the other.
    pub fn last_input_tokens(&self) -> u32 {
        self.last_input_tokens
    }

    /// Total tool executions this session.
    pub fn tool_calls(&self) -> u32 {
        self.tool_calls
    }

    /// Tool executions that returned a value (not an error payload).
    pub fn tool_success(&self) -> u32 {
        self.tool_success
    }

    /// Tool executions that errored.
    pub fn tool_errors(&self) -> u32 {
        self.tool_errors
    }

    /// Reset the tally (a new session / clear). The host calls this when the
    /// user starts a fresh session so the cumulative figure reflects only the
    /// current session, not the runner's whole lifetime.
    pub fn reset(&mut self) {
        *self = UsageAccumulator::default();
    }
}

impl Default for UsageAccumulator {
    fn default() -> Self {
        Self {
            // Seed the provenance fold with the AND identity: a sum of zero
            // measured turns is complete, so the first recorded turn decides
            // whether the tally stays reported or turns unreported.
            cumulative: Usage {
                provenance: UsageProvenance::ALL_REPORTED,
                ..Usage::default()
            },
            last_input_tokens: 0,
            tool_calls: 0,
            tool_success: 0,
            tool_errors: 0,
        }
    }
}

/// A shared accumulator the runner hands to spawned run tasks (writer) and
/// reads from status_snapshot (reader).
pub(crate) type SharedUsage = Arc<Mutex<UsageAccumulator>>;

impl Runner {
    /// Start the session's usage tally from the turns its own log already
    /// holds. Called once at assembly, so a runner that reopened a session
    /// reports that session's totals: without it the cumulative covers only
    /// the turns since the reopen, while the delegated half of the same
    /// figure covers the whole session.
    ///
    /// The runner reads the log itself rather than taking the turns from a
    /// caller: the tally and the log it answers for belong to one owner, so
    /// no assembly path can seed it from a span other than the session it was
    /// built for.
    ///
    /// The per-model table is seeded from the same turns, because a pane reads
    /// the session's cumulative and its per-model rows side by side: seeding
    /// one and not the other would leave two figures of one panel covering
    /// different spans.
    ///
    /// The turns come from the session's durable history page by page, not
    /// from the log state's tail: it holds the newest rows, so a tally
    /// read from it would cover the part of the session memory happens to hold
    /// while the pane reports it as the session's own.
    pub fn seed_usage_from_log(&self, session: SessionId) {
        let store = self.store.clone();
        let pages = TrajectoryPages::new(store.as_ref(), session, TrajectoryPosition::start());
        for page in pages {
            let page = match page {
                Ok(page) => page,
                Err(e) => {
                    tracing::warn!("usage seed read of session {session} stopped: {e}");
                    break;
                }
            };
            self.seed_usage_page(&page.events);
        }
    }

    /// Fold one page of a session's log into both tallies. Each accumulator is
    /// read under its own lock scope, never nested inside the other: the two
    /// are independent, and a reader of either holds only its own.
    fn seed_usage_page(&self, events: &[SessionLogEntry]) {
        match self.usage.lock() {
            Ok(mut g) => g.seed_from_events(events),
            // A poisoned lock means a panic in a writer; the tally stays at
            // zero, which is what a session with no readable turns reports.
            Err(_) => {
                tracing::warn!("usage accumulator lock poisoned; session totals start at zero")
            }
        }
        match self.observability.lock() {
            Ok(mut g) => g.seed_usage_from_events(events),
            Err(_) => {
                tracing::warn!("observability log lock poisoned; per-model totals start at zero")
            }
        }
    }

    /// A cheap point-in-time snapshot of the runner's live state for the host
    /// to render. Reads the breaker (when attached) and the shared usage
    /// accumulator; no await, no channel, safe to call on every dispatch.
    pub fn status_snapshot(&self) -> StatusSnapshot {
        let (breaker_state, breaker_reason, breaker_cool_down) = match &self.breaker {
            Some(b) => {
                let state = breaker_label(b.state());
                let reason = b.trip_reason().map(|r| format!("{r}"));
                let cool_down = b.cool_down_remaining();
                (Some(state), reason, cool_down)
            }
            None => (None, None, None),
        };
        let (cumulative_usage, last_input_tokens, tool_calls, tool_success, tool_errors) =
            match self.usage.lock() {
                Ok(g) => (
                    g.cumulative(),
                    g.last_input_tokens(),
                    g.tool_calls(),
                    g.tool_success(),
                    g.tool_errors(),
                ),
                // A poisoned lock means a panic during a run; report zeros rather
                // than propagate a second panic into the render path.
                Err(_) => (Usage::default(), 0, 0, 0, 0),
            };
        // Read the live model id + the window it resolves to, not the static
        // config/provider-caps values. A /model switch swaps active_model
        // (RwLock) and the next request resolves the new window; status must
        // reflect both so a post-switch /status does not show stale fields.
        let model = self.active_model();
        let resolved_caps = resolve_capabilities(
            &model,
            self.provider.capabilities(),
            self.catalog_resolver
                .as_deref()
                .and_then(|r| r.catalog_context_window(&model)),
        );
        StatusSnapshot {
            model,
            breaker_state,
            breaker_reason,
            breaker_cool_down,
            cumulative_usage,
            last_input_tokens,
            context_used_tokens: self.last_measurement().map(|m| m.token_count()),
            context_window: resolved_caps.context_window,
            tool_calls,
            tool_success,
            tool_errors,
        }
    }

    /// Per-model token breakdown for the Usage sub-tab's "Usage by model"
    /// section. Sorted by input+output descending so the heaviest model
    /// leads. Returns an empty vec when the observability log is empty or the
    /// lock is poisoned; the render path then degrades to the flat cumulative
    /// tally. The server trims this to the protocol view (no USD, no capability
    /// fields) before sending.
    pub fn by_model_usage(&self) -> Vec<(String, ModelUsage)> {
        let Ok(ol) = self.observability.lock() else {
            return Vec::new();
        };
        let mut entries: Vec<(String, ModelUsage)> = ol.cost().by_model.into_iter().collect();
        entries.sort_by(|a, b| {
            (b.1.input_tokens + b.1.output_tokens).cmp(&(a.1.input_tokens + a.1.output_tokens))
        });
        entries
    }

    /// Reset the cumulative usage tally and the per-model table together. The
    /// host calls this on /clear so the figures reflect the new session only:
    /// the two are read side by side in one panel, so clearing one and not the
    /// other would leave that panel covering two spans.
    pub fn reset_usage(&self) {
        if let Ok(mut g) = self.usage.lock() {
            g.reset();
        }
        if let Ok(mut g) = self.observability.lock() {
            g.reset_usage();
        }
    }

    /// Drop the cached context measurement, so the occupancy a surface reports
    /// after a clear is absent rather than the size of a context that no longer
    /// exists. The next turn assembles one.
    pub fn reset_measurement(&self) {
        self.context_builder.clear_measurement();
    }

    /// Drain the queued startup warnings (bad settings fields, network-policy
    /// typos) so the host can push them as initial transcript system lines at
    /// pair time. Synchronous + before the run loop, so the warnings land
    /// before any run output — no async-sink race with later assertions.
    /// Drains the queue so a second call returns empty.
    pub fn drain_startup_warnings(&self) -> Vec<String> {
        take(
            &mut *self
                .startup_warnings
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// Clear the session's in-memory log state and record the clear as
    /// a durable boundary marker.
    ///
    /// The host calls this on /clear. The log state is dropped first so the marker
    /// appended afterwards is the only event in the fresh log state: the boundary
    /// stays visible to an in-process reader while the pre-clear history is
    /// gone from it, and the durable log keeps the whole sequence.
    pub async fn reset_trajectory(&self, session: SessionId) {
        let prior_turn = match self.observability.lock() {
            Ok(ol) => ol.turn_coords().0,
            Err(_) => 0,
        };
        self.store.reset_trajectory(session);
        if let Err(e) = self
            .store
            .append(new_event(
                session,
                SessionEvent::ContextCleared { prior_turn },
            ))
            .await
        {
            tracing::warn!("context cleared append failed: {e}");
        }
    }

    /// The names + descriptions of every tool registered on this runner, in
    /// registry order. The /tools command renders this for capability
    /// discoverability — the host (and user) can see what the agent can do
    /// without reading source. Returns (name, description) pairs so the host
    /// never imports the provider's ToolDef type — the layering stays
    /// Presentation -> core -> provider.
    pub fn tools_snapshot(&self) -> Vec<(String, String)> {
        self.tools
            .tool_defs()
            .into_iter()
            .map(|d| (d.name, d.description))
            .collect()
    }

    /// List all registered hooks for the /hooks visibility command. Empty when
    /// no hook registry is configured. Returns core HookEntry (name, events,
    /// source); the server converts to the protocol DTO.
    pub fn hooks_list(&self) -> Vec<HookEntry> {
        self.hooks.as_ref().map(|r| r.list()).unwrap_or_default()
    }

    /// List model-invocable skills paired with their discovery origin, for
    /// the /skills visibility command. The server converts to the protocol DTO.
    /// Empty when no skill registry is configured.
    pub fn skills_snapshot(&self) -> Vec<SkillSnapshot> {
        self.skill_registry
            .as_ref()
            .map(|r| r.list_with_origin())
            .unwrap_or_default()
    }

    /// Snapshot of flagged redundant tool calls this session, for the
    /// /trajectory pane (the self-evolution reward signal surface). Clones
    /// the records (they stay for future queries); empty when the tracker
    /// caught nothing.
    pub fn redundancy_snapshot(&self) -> Vec<RedundantCall> {
        self.redundancy
            .lock()
            .map(|t| t.records().iter().cloned().collect())
            .unwrap_or_default()
    }

    /// List every stored memory as a frontmatter-only summary (no body), for
    /// the /memory command. Empty when no memory provider is configured.
    pub fn memory_list(&self) -> Vec<houyicoder_context::MemorySummary> {
        self.memory.list()
    }

    /// Fetch the full body of one memory by key, for /memory <key>. None when
    /// no provider is configured or the key is absent.
    pub fn memory_show(&self, key: &str) -> Option<houyicoder_context::MemoryEntry> {
        self.memory.show(key)
    }

    /// Forget one memory by key + scope (the /memory pane d action + /memory
    /// forget command). The scope label (user / project / auto) routes the
    /// delete to the matching storage root so forgetting a user/project row
    /// deletes the explicit file, not just the auto-scope copy. Returns Err
    /// when no provider is configured or the delete fails (absent key, bad path).
    /// The caller re-lists to refresh the pane.
    pub fn memory_forget(
        &self,
        key: &str,
        scope: &str,
    ) -> Result<(), houyicoder_context::MemoryError> {
        self.memory.forget(key, scope)
    }

    /// Read the memory gate state (auto-memory, auto-dream) for the /memory
    /// pane. The switches are runtime-flippable atomics shared with the
    /// drive loop, so a change lands on the next gate check with no restart.
    /// Pure read; the caller owns persistence (the layering keeps the config
    /// crate out of core, so the persist call lives in the service boundary).
    pub fn memory_gate_state(&self) -> MemoryGateState {
        self.memory.gate_state()
    }

    /// Set the auto-memory switch. The drive loop gates turn-entry recall
    /// and the background extractor on this same atomic, so the change takes
    /// effect on the next gate check. Does not persist; the caller writes
    /// the resulting state to the settings file.
    pub fn set_auto_memory(&self, enabled: bool) {
        self.memory.set_auto_memory(enabled);
    }

    /// Set the auto-dream switch. The drive loop gates the background
    /// consolidation dream on this same atomic, so the change takes effect
    /// on the next gate check. Does not persist.
    pub fn set_auto_dream(&self, enabled: bool) {
        self.memory.set_auto_dream(enabled);
    }

    /// The measurement of the most recent turn. None before this process has
    /// run one. Carries the sections, which is what the reported occupancy
    /// reads; the /context pane measures the session instead.
    pub fn last_measurement(&self) -> Option<ContextMeasurement> {
        self.context_builder.last_measurement()
    }

    /// Measure the session as the store holds it: the system prompt, the
    /// declared tools, the announced skills, the conversation selected through
    /// the session's latest checkpoint manifest, and what its tools and
    /// children cost. The /context view reads this, so a resumed session
    /// reports its history before a new message is sent and a live one reports
    /// the turns it has run.
    pub async fn session_measurement(
        &self,
        session: SessionId,
    ) -> Result<ContextMeasurement, RunError> {
        let view = self.store.current_view(session).await?;
        let parts = self.assembly_parts();
        let assembled = self.context_builder.measure(
            &view.events,
            view.manifest.as_ref(),
            Some(self.store.backend()),
            &parts.input(),
        );
        Ok(assembled.measurement)
    }

    /// Install recovery state for destructive operations.
    /// The composition root calls this after constructing the runner, passing
    /// the same Arc handles it gave the BashTool so push (BashTool) and pop
    /// (undo_last) share one stack.
    pub fn set_undo(
        &mut self,
        undo_stack: Arc<Mutex<UndoStack>>,
        snapshot_store: Arc<SnapshotStore>,
    ) {
        self.undo_stack = Some(undo_stack);
        self.snapshot_store = Some(snapshot_store);
    }

    /// Undo the most recent recoverable operation. Peeks the top entry,
    /// restores from it, and only pops on success — a restore failure
    /// leaves the entry on the stack so the user can retry. Returns a
    /// typed outcome distinguishing empty stack, success, and failure.
    pub fn undo_last(&self) -> UndoOutcome {
        let Some(stack) = &self.undo_stack else {
            return UndoOutcome::Empty;
        };
        let Some(store) = &self.snapshot_store else {
            return UndoOutcome::Empty;
        };
        let Ok(mut guard) = stack.lock() else {
            return UndoOutcome::Empty;
        };
        let Some(entry) = guard.peek() else {
            return UndoOutcome::Empty;
        };
        match store.restore(entry) {
            Ok(()) => {
                let entry = guard.pop().expect("peeked entry exists after restore");
                UndoOutcome::Restored(entry)
            }
            Err(e) => UndoOutcome::Failed(e.to_string()),
        }
    }

    /// Prune expired snapshots + enforce a size cap. Called from Runner::run()
    /// at the start of each run. Snapshots referenced by the undo stack
    /// are protected from pruning.
    pub fn prune_snapshots(&self) {
        let Some(store) = &self.snapshot_store else {
            return;
        };
        let Some(stack) = &self.undo_stack else {
            return;
        };
        let protected = stack
            .lock()
            .ok()
            .map(|s| s.snapshot_paths())
            .unwrap_or_default();
        store.prune(
            self.snapshot_ttl_secs,
            self.snapshot_size_cap_bytes,
            &protected,
        );
    }

    /// The undo stack depth (for the host to show undoable ops count).
    pub fn undo_depth(&self) -> usize {
        self.undo_stack
            .as_ref()
            .and_then(|s| s.lock().ok())
            .map(|s| s.len())
            .unwrap_or(0)
    }
}

/// Map a breaker state to a render label. Kept here (in core) so the host
/// receives a string and never names the resilience enum — the layering stays
/// Presentation -> core -> resilience.
fn breaker_label(state: BreakerState) -> &'static str {
    match state {
        BreakerState::Closed => "Closed",
        BreakerState::Open => "Open",
        BreakerState::HalfOpen => "HalfOpen",
    }
}

#[cfg(test)]
mod tests;
