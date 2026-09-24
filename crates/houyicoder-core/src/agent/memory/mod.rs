//! Owns memory access, feature gates, preservation, and background work.

mod gates;
mod mutation_log;
mod preservation;
mod recall;
pub(crate) mod selector;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use houyicoder_api::agent_event::{
    AgentEventHandlers, EventHandler, MemoryChangeCausality, MemoryChangeOrigin, MemoryChangedEvent,
};
use houyicoder_api::memory::{MemoryProvider, MemoryReranker};
use houyicoder_api::session::SessionLog;
use houyicoder_context::{
    CheckpointManifest, EventId, MemoryEntry, MemoryError, MemoryScope, MemorySummary, SessionId,
    SessionLogEntry,
};

pub use gates::{MemoryGateState, MemoryGates};
pub(crate) use mutation_log::MutationLog;
pub(crate) use preservation::{preserve_folded_context, preserve_session};
use selector::RecallTasks;

use crate::agent::auto_dream::DreamRunner;
use crate::agent::extractor::MemoryExtractor;
use crate::agent::reward_snapshot::RewardSnapshot;

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
    pub(crate) async fn recall(&self, session: SessionId) -> Result<(), crate::agent::RunError> {
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

    /// Preserve memory candidates from events the manifest marks Summarized
    /// before compaction folds them out. Best-effort: a write failure logs
    /// and continues; memory never blocks the compaction path.
    pub(crate) fn preserve_folded(
        &self,
        events: &[SessionLogEntry],
        manifest: &CheckpointManifest,
    ) {
        let Some(memory) = &self.provider else {
            return;
        };
        let existing: std::collections::HashSet<String> =
            memory.list_memories().into_iter().map(|s| s.key).collect();
        for entry in preserve_folded_context(events, manifest) {
            if !existing.contains(&entry.key)
                && let Err(error) = memory.add(entry)
            {
                tracing::warn!("before-compact preservation write failed: {error}");
            }
        }
    }

    /// Preserve memory candidates from the whole session before /clear.
    /// Best-effort: a write failure logs and continues; memory never blocks
    /// the clear path.
    pub(crate) async fn preserve_before_clear(
        &self,
        session: SessionId,
    ) -> Result<(), crate::agent::RunError> {
        self.invalidate_index_snapshot();
        let Some(memory) = &self.provider else {
            return Ok(());
        };
        let events = self.store.replay(session).await?;
        let existing: std::collections::HashSet<String> =
            memory.list_memories().into_iter().map(|s| s.key).collect();
        for entry in preserve_session(&events) {
            if existing.contains(&entry.key) {
                continue;
            }
            if let Err(e) = memory.add(entry) {
                tracing::warn!("before-clear preservation write failed: {e}");
            }
        }
        Ok(())
    }

    /// Format the memory index for the system prompt prefix. The first result
    /// is held until clear or compact so background memory writes cannot change
    /// the provider prefix between ordinary turns.
    pub(crate) fn format_index(&self) -> Option<String> {
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
        Some(
            summaries
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
                .collect(),
        )
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
        if self.gates.auto_memory_enabled()
            && let Some(extractor) = self.background.extractor.as_ref()
        {
            match self.store.replay(session).await {
                Ok(messages) => extractor.extract_memories(messages),
                Err(error) => tracing::warn!("memory extract replay failed: {error}"),
            }
        }
        if self.gates.auto_dream_enabled()
            && let Some(dream) = self.background.dream.as_ref()
        {
            dream.execute_dream(reward.map(|f| f()), Some(&session.to_string()));
        }
    }

    /// Await running background tasks — recall selections first, then dream
    /// passes — until they finish or the timeout expires.
    pub(crate) async fn join_background(&self, timeout: Duration) {
        self.recall_tasks.drain(timeout).await;
        if let Some(dream) = self.background.dream.as_ref() {
            dream.drain_pending(timeout).await;
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
                id: houyicoder_context::MemoryChangeId::new(),
                origin: MemoryChangeOrigin::PrimaryAgent,
                // The drain runs while the leg's turn is still current, a pause
                // included, so the notice is attributed to that turn.
                causality: MemoryChangeCausality::ThisTurn,
                changes,
            });
        }
    }

    /// The dream's cross-session scan root, or None when in-memory.
    pub(crate) fn dream_session_log_root(&self) -> Option<&std::path::Path> {
        self.background
            .dream
            .as_ref()
            .and_then(|d| d.session_log_root.as_deref())
    }
}
