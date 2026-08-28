//! agent::status — the read-only snapshot a host reads to render the
//! session-health commands (/context, /status, /sandbox). The host asks the
//! runner for what only the engine knows (breaker state + cumulative usage +
//! context window); it overlays the mode and session id it owns itself. This
//! keeps the dependency direction honest: Presentation -> core -> resilience,
//! the host never reaches past the engine into the resilience layer to read
//! breaker state directly.
//!
//! The snapshot is cheap to build: one breaker lock, one accumulator lock,
//! one provider capability call. No await, no channel. The host may call it
//! on every command dispatch without throttling.

use std::sync::{Arc, Mutex};

use houyicoder_protocol::llm::Usage;
use houyicoder_resilience::BreakerState;

use crate::observability::MetricsView;

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
    pub breaker_cool_down: Option<std::time::Duration>,
    /// Cumulative provider-reported usage across every turn this runner has
    /// driven (since the accumulator was last reset). 7-field inclusive totals
    /// plus the non-overlapping breakdown; see the protocol layer's Usage doc.
    pub cumulative_usage: Usage,
    /// The input_tokens of the last response — a proxy for how full the model
    /// context window is right now (the agent footprint at the last call).
    pub last_input_tokens: u32,
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
#[derive(Debug, Default)]
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

    /// The last response's input_tokens (current window footprint proxy). The
    /// status bar divides this by the context window for its fill %. The
    /// observability log's context_pct is the SAME formula + source (per-turn
    /// input_tokens / window); they stay separate fields so the status bar
    /// reads the live snapshot while the log records the per-turn delta, but
    /// the two numbers must not diverge — a future "fix" to one must apply to
    /// both.
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
        self.cumulative = Usage::default();
        self.last_input_tokens = 0;
        self.tool_calls = 0;
        self.tool_success = 0;
        self.tool_errors = 0;
    }
}

/// A shared accumulator the runner hands to spawned run tasks (writer) and
/// reads from status_snapshot (reader).
pub(crate) type SharedUsage = Arc<Mutex<UsageAccumulator>>;

impl crate::agent::Runner {
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
        let resolved_caps =
            super::model_window::resolve_capabilities(&model, self.provider.capabilities());
        StatusSnapshot {
            model,
            breaker_state,
            breaker_reason,
            breaker_cool_down,
            cumulative_usage,
            last_input_tokens,
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
    /// tally. The server trims this to the wire view (no USD, no capability
    /// fields) before sending.
    pub fn by_model_usage(&self) -> Vec<(String, crate::observability::ModelUsage)> {
        let Ok(ol) = self.observability.lock() else {
            return Vec::new();
        };
        let mut entries: Vec<(String, crate::observability::ModelUsage)> =
            ol.cost().by_model.into_iter().collect();
        entries.sort_by(|a, b| {
            (b.1.input_tokens + b.1.output_tokens).cmp(&(a.1.input_tokens + a.1.output_tokens))
        });
        entries
    }

    /// Reset the cumulative usage tally. The host calls this on /clear so the
    /// tally reflects the new session only.
    pub fn reset_usage(&self) {
        if let Ok(mut g) = self.usage.lock() {
            g.reset();
        }
    }

    /// Drain the queued startup warnings (bad settings fields, network-policy
    /// typos) so the host can push them as initial transcript system lines at
    /// pair time. Synchronous + before the run loop, so the warnings land
    /// before any run output — no async-sink race with later assertions.
    /// Drains the queue so a second call returns empty.
    pub fn drain_startup_warnings(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .startup_warnings
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// Read the in-memory trajectory buffer for a session (sync): the
    /// finalized events in append order, each with prev_hash set. The
    /// /trajectory command projects this. Empty for a session with no
    /// events appended this process.
    pub fn trajectory_snapshot(
        &self,
        session: houyicoder_context::SessionId,
    ) -> Vec<houyicoder_context::TurnEvent> {
        self.store.trajectory_snapshot(session)
    }

    /// Drop the in-memory trajectory buffer for a session. The host calls
    /// this on /clear so /trajectory reads fresh after a clear.
    pub fn reset_trajectory(&self, session: houyicoder_context::SessionId) {
        self.store.reset_trajectory(session);
    }

    /// The names + descriptions of every tool registered on this runner, in
    /// registry order. The /tools command projects this for capability
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
    /// no hook registry is wired. Returns core HookEntry (name, events,
    /// source); the server converts to the wire DTO.
    pub fn hooks_list(&self) -> Vec<crate::agent::hook::registry::HookEntry> {
        self.hooks.as_ref().map(|r| r.list()).unwrap_or_default()
    }

    /// List model-invocable skills paired with their discovery origin, for
    /// the /skills visibility command. The server converts to the wire DTO.
    /// Empty when no skill registry is wired.
    pub fn skills_snapshot(&self) -> Vec<houyicoder_api::skill::SkillSnapshot> {
        self.skill_registry
            .as_ref()
            .map(|r| r.list_with_origin())
            .unwrap_or_default()
    }

    /// Snapshot of flagged redundant tool calls this session, for the
    /// /trajectory pane (the self-evolution reward signal surface). Clones
    /// the records (they stay for future queries); empty when the tracker
    /// caught nothing.
    pub fn redundancy_snapshot(&self) -> Vec<crate::observability::evolution::RedundantCall> {
        self.redundancy
            .lock()
            .map(|t| t.records().iter().cloned().collect())
            .unwrap_or_default()
    }

    /// List every stored memory as a frontmatter-only summary (no body), for
    /// the /memory command. Empty when no memory provider is wired.
    pub fn memory_list(&self) -> Vec<houyicoder_context::MemorySummary> {
        self.memory
            .as_ref()
            .map(|m| m.list_memories())
            .unwrap_or_default()
    }

    /// Fetch the full body of one memory by key, for /memory <key>. None when
    /// no provider is wired or the key is absent.
    pub fn memory_show(&self, key: &str) -> Option<houyicoder_context::MemoryEntry> {
        self.memory.as_ref().and_then(|m| m.show_memory(key))
    }

    /// Forget one memory by key + scope (the /memory pane d action + /memory
    /// forget command). The scope label (user / project / auto) routes the
    /// delete to the matching storage root so forgetting a user/project row
    /// deletes the explicit file, not just the auto-scope copy. Returns Err
    /// when no provider is wired or the delete fails (absent key, bad path).
    /// The caller re-lists to refresh the pane.
    pub fn memory_forget(
        &self,
        key: &str,
        scope: &str,
    ) -> Result<(), houyicoder_context::MemoryError> {
        let Some(memory) = &self.memory else {
            return Ok(());
        };
        // Map the wire label to a scope; an unknown label (a client bug)
        // falls back to Auto, the single-root behavior, so a bad label never
        // panics the dispatch path.
        let scope = houyicoder_context::MemoryScope::from_label(scope)
            .unwrap_or(houyicoder_context::MemoryScope::Auto);
        memory.delete_memory_in_scope(key, scope)
    }

    /// Read both memory toggles (auto-memory, auto-dream) for the /memory pane.
    /// The switches are runtime-flippable atomics shared with the drive loop, so
    /// a flip lands on the next gate check with no restart. Pure read; the
    /// caller owns persistence (the layering keeps the config crate out of
    /// core, so the persist call lives in the service boundary).
    pub fn toggles_state(&self) -> (bool, bool) {
        use std::sync::atomic::Ordering;
        (
            self.auto_memory.load(Ordering::Relaxed),
            self.auto_dream.load(Ordering::Relaxed),
        )
    }

    /// Flip the auto-memory switch and return the new value. The drive loop
    /// gates turn-entry recall + the background extractor on this same atomic,
    /// so the flip takes effect on the next gate check. Does not persist; the
    /// caller writes the resulting pair to the settings file.
    pub fn flip_auto_memory(&self) -> bool {
        use std::sync::atomic::Ordering;
        let next = !self.auto_memory.load(Ordering::Relaxed);
        self.auto_memory.store(next, Ordering::Relaxed);
        next
    }

    /// Flip the auto-dream switch and return the new value. The drive loop
    /// gates the background consolidation dream on this same atomic, so the
    /// flip takes effect on the next gate check. Does not persist.
    pub fn flip_auto_dream(&self) -> bool {
        use std::sync::atomic::Ordering;
        let next = !self.auto_dream.load(Ordering::Relaxed);
        self.auto_dream.store(next, Ordering::Relaxed);
        next
    }

    /// The most recently built served view (cached by ContextBuilder after each
    /// model call), so /context renders the real per-section breakdown the model
    /// saw — not a stub. None before the first turn or when no view has been
    /// built this process; the host falls back to the stub path then.
    pub fn context_served(&self) -> Option<crate::agent::ServedView> {
        self.context_builder.last_served()
    }

    /// A prospective served view — what the model would see on the first turn
    /// (system prompt + tools + memory sections, messages = 0). Used when
    /// context_served() is None (fresh session, no turn run yet) so /context is
    /// never empty. Builds with an empty event slice: messages section is 0,
    /// memory recall uses an empty query (no entries surfaced), the system
    /// prompt and tools sections carry their real token counts.
    pub fn context_prospective(&self) -> crate::agent::ServedView {
        self.context_builder.build(&[])
    }

    /// Wire the undo stack + snapshot store for recoverable destructive ops.
    /// The composition root calls this after constructing the runner, passing
    /// the same Arc handles it gave the BashTool so push (BashTool) and pop
    /// (undo_last) share one stack.
    pub fn set_undo(
        &mut self,
        undo_stack: Arc<std::sync::Mutex<crate::snapshot::UndoStack>>,
        snapshot_store: Arc<crate::snapshot::SnapshotStore>,
    ) {
        self.undo_stack = Some(undo_stack);
        self.snapshot_store = Some(snapshot_store);
    }

    /// Undo the most recent recoverable operation. Peeks the top entry,
    /// restores from it, and only pops on success — a restore failure
    /// leaves the entry on the stack so the user can retry. Returns a
    /// typed outcome distinguishing empty stack, success, and failure.
    pub fn undo_last(&self) -> crate::snapshot::UndoOutcome {
        let Some(stack) = &self.undo_stack else {
            return crate::snapshot::UndoOutcome::Empty;
        };
        let Some(store) = &self.snapshot_store else {
            return crate::snapshot::UndoOutcome::Empty;
        };
        let Ok(mut guard) = stack.lock() else {
            return crate::snapshot::UndoOutcome::Empty;
        };
        let Some(entry) = guard.peek() else {
            return crate::snapshot::UndoOutcome::Empty;
        };
        match store.restore(entry) {
            Ok(()) => {
                let entry = guard.pop().expect("peeked entry exists after restore");
                crate::snapshot::UndoOutcome::Restored(entry)
            }
            Err(e) => crate::snapshot::UndoOutcome::Failed(e.to_string()),
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
#[cfg(test)]
#[path = "status_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "status_override_tests.rs"]
mod status_override_tests;
