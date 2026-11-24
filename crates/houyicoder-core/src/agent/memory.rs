//! Owns memory access, feature gates, preservation, and background work.

pub mod auto_dream;
pub mod extract;
pub(crate) mod extraction_window;
pub mod extractor;
pub(crate) mod fact;
pub(crate) mod failure_recall;
pub(crate) mod reward_snapshot;

mod gates;
mod mutation_log;
mod recall;
pub(crate) mod selector;

use std::collections::HashSet;
use std::mem::take;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::timeout_at;

/// How long a turn's background pipeline waits for the extraction pass before
/// starting the dream. The dream only consolidates what the store holds, so a
/// pass that outlives this budget is consolidated by the next dream instead.
const EXTRACTION_BEFORE_DREAM: Duration = Duration::from_secs(60);

use houyicoder_api::agent_event::{
    AgentEventHandlers, EventHandler, MemoryChangeCausality, MemoryChangeOrigin, MemoryChangedEvent,
};
use houyicoder_api::memory::{MemoryProvider, MemoryReranker};
use houyicoder_api::session::SessionLog;
use houyicoder_context::{
    EventId, MemoryEntry, MemoryError, MemoryScope, MemorySummary, SessionId, memory_index_row,
    render_memory_index_groups, tokens_for,
};

pub use gates::{MemoryGateState, MemoryGates};
pub(crate) use mutation_log::MutationLog;
use selector::RecallTasks;

use crate::agent::RunError;
use crate::agent::auto_dream::DreamRunner;
use crate::agent::extractor::MemoryExtractor;
use crate::agent::reward_snapshot::RewardSnapshot;
use houyicoder_context::MemoryChangeId;
use std::path::Path;

/// How many memories the system prompt index carries. The cap keeps a store
/// that grew without bound from crowding out the rest of the prompt.
pub(crate) const INDEX_ENTRY_CAP: usize = 200;

/// Token budget for the injected index. It is a budget rather than a byte or
/// line cap because the rows carry CJK summaries, where a byte cap stops far
/// fewer entries than it looks like it should, and because the index is paid
/// for on every request while only the opening rows earn their place.
pub(crate) const INDEX_TOKEN_BUDGET: usize = 4_000;

/// One line of the system prompt's memory index, the exact text a stored
/// memory contributes to the prompt. The frozen index is the only thing the
/// prompt carries, so the cost the measurement reports is the cost the request
/// pays.
pub(crate) fn index_line(summary: &MemorySummary) -> String {
    format!("{}\n", memory_index_row(&summary.key, &summary.description))
}

/// Render the injected index under one token budget. Selection runs first, so
/// scope priority decides what fits and a large automatic set cannot starve a
/// preference the user wrote. Grouping runs once over the selected rows, so
/// each type heading appears at most once under the section heading.
pub(crate) fn render_index(summaries: &[MemorySummary]) -> String {
    let mut selected: Vec<&MemorySummary> = Vec::new();
    let mut used = 0usize;
    for scope in [MemoryScope::User, MemoryScope::Project, MemoryScope::Auto] {
        for summary in summaries.iter().filter(|s| s.scope == scope) {
            if selected.len() >= INDEX_ENTRY_CAP {
                break;
            }
            let cost = tokens_for(&index_line(summary));
            if used + cost > INDEX_TOKEN_BUDGET {
                break;
            }
            used += cost;
            selected.push(summary);
        }
    }
    render_memory_index_groups(selected.iter().map(|summary| {
        (
            summary.source,
            summary.key.as_str(),
            summary.description.as_str(),
        )
    }))
}

/// Optional background memory workers.
struct BackgroundMemory {
    extractor: Option<Arc<MemoryExtractor>>,
    dream: Option<Arc<DreamRunner>>,
}

impl BackgroundMemory {
    fn none() -> Self {
        Self {
            extractor: None,
            dream: None,
        }
    }
}

#[derive(Default)]
enum MemoryIndexSnapshot {
    #[default]
    Uninitialized,
    Loaded(Option<String>),
}

/// Coordinates memory state and lifecycle operations.
pub struct MemoryRuntime {
    store: Arc<dyn SessionLog>,
    provider: Option<Arc<dyn MemoryProvider>>,
    reranker: Option<Arc<dyn MemoryReranker>>,
    gates: MemoryGates,
    background: BackgroundMemory,
    index_snapshot: Mutex<MemoryIndexSnapshot>,
    primary_recorder: Option<Arc<MutationLog>>,
    memory_changed: Mutex<Option<Arc<dyn EventHandler<MemoryChangedEvent>>>>,
    recall_tasks: RecallTasks,
    /// Error signatures already recalled in this turn, so one failure does
    /// not inject the same lesson again at every later step of the turn.
    failure_signatures: Mutex<HashSet<String>>,
    /// Handles of the per-turn background pipelines, reaped on the next fire
    /// and joined at shutdown so an extraction started by the last turn is
    /// not cut off by process exit.
    background_pipeline: Mutex<Vec<JoinHandle<()>>>,
    /// Serializes the turn pipelines. Two turns can end within seconds of
    /// each other; without this lock the second pipeline could start the
    /// dream beside the first turn's still-running extraction.
    background_lock: Arc<tokio::sync::Mutex<()>>,
}

impl MemoryRuntime {
    /// Construct an enabled runtime without a provider or background workers.
    pub fn new(store: Arc<dyn SessionLog>) -> Self {
        Self {
            store,
            provider: None,
            reranker: None,
            gates: MemoryGates::new(true, true),
            background: BackgroundMemory::none(),
            index_snapshot: Mutex::new(MemoryIndexSnapshot::Uninitialized),
            primary_recorder: None,
            memory_changed: Mutex::new(None),
            recall_tasks: RecallTasks::default(),
            failure_signatures: Mutex::new(HashSet::new()),
            background_pipeline: Mutex::new(Vec::new()),
            background_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Construct a configured runtime from its collaborators.
    pub fn from_parts(
        store: Arc<dyn SessionLog>,
        provider: Option<Arc<dyn MemoryProvider>>,
        gates: MemoryGates,
        extractor: Option<Arc<MemoryExtractor>>,
        dream: Option<Arc<DreamRunner>>,
    ) -> Self {
        Self {
            store,
            provider,
            reranker: None,
            gates,
            background: BackgroundMemory { extractor, dream },
            index_snapshot: Mutex::new(MemoryIndexSnapshot::Uninitialized),
            primary_recorder: None,
            memory_changed: Mutex::new(None),
            recall_tasks: RecallTasks::default(),
            failure_signatures: Mutex::new(HashSet::new()),
            background_pipeline: Mutex::new(Vec::new()),
            background_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Last message id the extraction consumed or was seeded to, or None
    /// with no extractor configured or nothing consumed yet.
    pub fn extractor_cursor(&self) -> Option<EventId> {
        self.background.extractor.as_ref().and_then(|e| e.cursor())
    }

    /// The turn frontier the wired extractor classifies a pass against: the
    /// session's latest durable user input, or None with no extractor
    /// configured or no user input yet.
    pub fn extractor_frontier(&self, session: SessionId) -> Option<EventId> {
        self.background
            .extractor
            .as_ref()
            .and_then(|e| e.turn_frontier(session))
    }

    /// Return the configured provider.
    pub(crate) fn provider(&self) -> Option<&Arc<dyn MemoryProvider>> {
        self.provider.as_ref()
    }

    /// Install a provider during crate-internal incremental assembly.
    pub(crate) fn install_provider(&mut self, provider: Arc<dyn MemoryProvider>) {
        self.provider = Some(provider);
        self.invalidate_index_snapshot();
    }

    /// Return the semantic selection stage, when one is installed.
    pub(crate) fn reranker(&self) -> Option<&Arc<dyn MemoryReranker>> {
        self.reranker.as_ref()
    }

    /// Install the semantic selection stage during composition. Without it a
    /// weak lexical signal falls back deterministically instead of selecting.
    pub fn install_reranker(&mut self, reranker: Arc<dyn MemoryReranker>) {
        self.reranker = Some(reranker);
    }

    /// Read the gate state snapshot.
    pub(crate) fn gate_state(&self) -> MemoryGateState {
        self.gates.state()
    }

    pub(crate) fn set_auto_memory(&self, enabled: bool) {
        self.gates.set_auto_memory(enabled);
    }

    pub(crate) fn set_auto_dream(&self, enabled: bool) {
        self.gates.set_auto_dream(enabled);
    }

    /// Recall relevant memory for the turn about to start. No-op when
    /// auto_memory is off, no provider is configured, or the selection
    /// settles on nothing. A triggered semantic stage runs beside the model
    /// call and appends its recall on completion.
    pub(crate) async fn recall(&self, session: SessionId) -> Result<(), RunError> {
        // A new user turn re-arms failure recall: the same error may happen
        // again in the next task and deserves its lesson then.
        self.failure_signatures
            .lock()
            .expect("failure signatures")
            .clear();
        recall::recall(
            &self.store,
            self.provider.as_ref(),
            self.reranker.as_ref(),
            &self.recall_tasks,
            &self.gates,
            session,
        )
        .await
    }

    /// Re-inject after compaction with the summary's own words added to the
    /// query. The summary carries the backbone's active file names, so the
    /// recall looks for memories about the work the compacted span covered,
    /// not just the last sentence the user typed.
    pub(crate) async fn recall_after_compaction(&self, session: SessionId) -> Result<(), RunError> {
        recall::recall_after_compaction(
            &self.store,
            self.provider.as_ref(),
            self.reranker.as_ref(),
            &self.recall_tasks,
            &self.gates,
            session,
        )
        .await
    }

    /// Drop selection tasks still running for this session. The run that
    /// spawned them has ended, so a late selection can only append a recall
    /// event the turn's requests have already passed.
    pub(crate) fn cancel_recall(&self, session: SessionId) {
        self.recall_tasks.cancel_session(session);
    }

    /// System lifecycle boundary: compaction is about to fold events out of
    /// the model's view. Hands the session to the background extraction
    /// pipeline so facts worth keeping are saved before the fold. This is
    /// system behavior, not a user hook: no registry dispatch, and the
    /// auto-memory gate is the only switch. Fire-and-forget; a failed job
    /// retries off the durable log and never blocks the compaction path.
    pub(crate) async fn before_compact(&self, session: SessionId) {
        self.submit_preservation(session).await;
    }

    /// System lifecycle boundary: clear is about to drop the session from
    /// the working view. Invalidates the cached index so the next prompt
    /// rebuilds from the post-clear store, then hands the session to the
    /// same extraction pipeline as compaction. Fire-and-forget; the clear
    /// proceeds regardless of preservation outcome.
    pub(crate) async fn before_clear(&self, session: SessionId) {
        self.invalidate_index_snapshot();
        self.submit_preservation(session).await;
    }

    /// Replay the durable session log and submit it to the extractor. Shared
    /// by the end-of-turn background fire and both context-shrink boundaries,
    /// so every extraction path honors the same gate and cursor. The log is
    /// the input snapshot: it is append-only and survives the boundaries, so
    /// a job that errors leaves the cursor unmoved and the next submission
    /// reconsiders the same range. A runtime without an extractor degrades
    /// to a no-op.
    async fn submit_preservation(&self, session: SessionId) {
        if self.gates.auto_memory_enabled()
            && let Some(extractor) = self.background.extractor.as_ref()
        {
            match self.store.replay(session).await {
                Ok(messages) => extractor.extract_memories(messages),
                Err(error) => tracing::warn!("memory extract replay failed: {error}"),
            }
        }
    }

    /// Format the memory index for the session prompt suffix. The first result
    /// is held until clear or compact so background memory writes cannot change
    /// the provider prefix between ordinary turns. A disabled auto-memory gate
    /// injects nothing and leaves the snapshot untouched, so re-enabling
    /// rebuilds from the store instead of replaying the disabled era.
    pub(crate) fn format_index(&self) -> Option<String> {
        self.index_snapshot()
    }

    /// The frozen index, building and storing it on first use. None when the
    /// gate is off, no provider is configured, or the store is empty.
    fn index_snapshot(&self) -> Option<String> {
        if !self.gates.auto_memory_enabled() {
            return None;
        }
        let Ok(mut snapshot) = self.index_snapshot.lock() else {
            tracing::warn!("memory index snapshot lock poisoned");
            return self.build_index();
        };
        match &*snapshot {
            MemoryIndexSnapshot::Loaded(index) => index.clone(),
            MemoryIndexSnapshot::Uninitialized => {
                let index = self.build_index();
                *snapshot = MemoryIndexSnapshot::Loaded(index.clone());
                index
            }
        }
    }

    fn build_index(&self) -> Option<String> {
        let memory = self.provider.as_ref()?;
        let summaries = memory.list_memories();
        if summaries.is_empty() {
            return None;
        }
        Some(render_index(&summaries))
    }

    /// Refresh the frozen index at a natural prompt-cache invalidation point.
    pub(crate) fn invalidate_index_snapshot(&self) {
        if let Ok(mut snapshot) = self.index_snapshot.lock() {
            *snapshot = MemoryIndexSnapshot::Uninitialized;
        } else {
            tracing::warn!("memory index snapshot lock poisoned during invalidation");
        }
    }

    /// List every stored memory as a frontmatter-only summary. Empty when
    /// no provider is configured.
    pub(crate) fn list(&self) -> Vec<MemorySummary> {
        self.provider
            .as_ref()
            .map(|m| m.list_memories())
            .unwrap_or_default()
    }

    /// Fetch the full body of one memory by key. None when no provider is
    /// configured or the key is absent.
    pub(crate) fn show(&self, key: &str) -> Option<MemoryEntry> {
        self.provider.as_ref().and_then(|m| m.show_memory(key))
    }

    /// Forget one memory by key and scope. Returns Ok when no provider is
    /// configured (no-op) or Err when the delete fails.
    pub(crate) fn forget(&self, key: &str, scope: &str) -> Result<(), MemoryError> {
        let Some(memory) = &self.provider else {
            return Ok(());
        };
        let scope = MemoryScope::from_label(scope).unwrap_or(MemoryScope::Auto);
        memory.delete_memory_in_scope(key, scope)
    }

    /// Start enabled background memory work after a final output.
    ///
    /// reward is Some(closure) when reward capture is on and None when an
    /// operator suppressed it (the HOUYICODER_REWARD_OFF switch). Reward
    /// capture feeds only the dream reward-driven gate; the extractor is a
    /// memory function and always runs. The closure is evaluated only when
    /// the dream block fires (auto-dream on and a dream worker wired), ahead
    /// of execute_dream running.
    pub(crate) async fn fire_background<F>(&self, session: SessionId, reward: Option<F>)
    where
        F: FnOnce() -> RewardSnapshot,
    {
        self.submit_preservation(session).await;
        let Some(dream) = self
            .background
            .dream
            .as_ref()
            .filter(|_| self.gates.auto_dream_enabled())
            .cloned()
        else {
            return;
        };
        // One background pipeline per turn: the extraction pass lands first,
        // then the dream reads the store it just wrote. Firing both at once
        // let the dream plan against a listing the extractor was still
        // changing, and put two notices on one turn.
        let extractor = self.background.extractor.clone();
        let serial = Arc::clone(&self.background_lock);
        let reward = reward.map(|f| f());
        let handle = tokio::spawn(async move {
            let _serial = serial.lock().await;
            if let Some(extractor) = extractor {
                extractor.wait_idle(EXTRACTION_BEFORE_DREAM).await;
            }
            dream.execute_dream(reward);
        });
        let mut running = self.background_pipeline.lock().expect("pipeline");
        running.retain(|h| !h.is_finished());
        running.push(handle);
    }

    /// Await running background tasks — recall selections first, then dream
    /// passes — until they finish or the timeout expires.
    pub(crate) async fn join_background(&self, timeout: Duration) {
        // One deadline for every stage: each stage waiting its own full budget
        // would let shutdown take a multiple of it.
        let deadline = Instant::now() + timeout;
        let remaining = || deadline.saturating_duration_since(Instant::now());
        self.recall_tasks.drain(remaining()).await;
        let pipeline: Vec<JoinHandle<()>> =
            take(&mut *self.background_pipeline.lock().expect("pipeline"));
        for handle in pipeline {
            drop(timeout_at(deadline, handle).await);
        }
        if let Some(extractor) = self.background.extractor.as_ref() {
            extractor.wait_idle(remaining()).await;
        }
        if let Some(dream) = self.background.dream.as_ref() {
            dream.drain_pending(remaining()).await;
        }
    }

    /// Install memory-change handlers on background workers.
    pub(crate) fn set_event_handlers(&self, events: &AgentEventHandlers) {
        if let Some(extractor) = self.background.extractor.as_ref() {
            extractor.set_memory_changed_handler(events.memory_changed_handler());
        }
        if let Some(dream) = self.background.dream.as_ref() {
            dream.set_memory_changed_handler(events.memory_changed_handler());
        }
        if let Some(handler) = events.memory_changed_handler() {
            *self.memory_changed.lock().expect("memory_changed") = Some(handler);
        }
    }

    /// Create and install the primary recorder the main-agent save tool
    /// records into. The returned Arc is threaded into MemoryAddTool so a
    /// main-agent save is captured at call time, then drained when the turn
    /// settles by drain_primary_changes. Replaces the post-hoc durable-log
    /// scan for primary saves.
    pub(crate) fn install_primary_recorder(&mut self) -> Arc<MutationLog> {
        let recorder = Arc::new(MutationLog::new());
        self.primary_recorder = Some(Arc::clone(&recorder));
        recorder
    }

    /// Drain the primary recorder and emit a PrimaryAgent change event for
    /// every save the main agent landed since the last drain. The drain is
    /// best-effort: no handler or no recorder means no emission.
    pub(crate) fn drain_primary_changes(&self) {
        let Some(recorder) = self.primary_recorder.as_ref() else {
            return;
        };
        let changes = recorder.take();
        if changes.is_empty() {
            return;
        }
        let handler = self.memory_changed.lock().expect("memory_changed").clone();
        if let Some(handler) = handler {
            handler.handle(MemoryChangedEvent {
                id: MemoryChangeId::new(),
                origin: MemoryChangeOrigin::PrimaryAgent,
                // The drain runs while the leg's turn is still current, a pause
                // included, so the notice is attributed to that turn.
                causality: MemoryChangeCausality::ThisTurn,
                changes,
            });
        }
    }

    /// The dream's cross-session scan root, or None when in-memory.
    pub(crate) fn dream_session_log_root(&self) -> Option<&Path> {
        self.background
            .dream
            .as_ref()
            .and_then(|d| d.session_log_root.as_deref())
    }
}

#[cfg(test)]
mod tests;
