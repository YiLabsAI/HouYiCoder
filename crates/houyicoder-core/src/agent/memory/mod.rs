//! Owns memory access, feature gates, preservation, and background work.

mod gates;
mod mutation_log;
mod preservation;
mod recall;

use std::sync::Arc;
use std::time::Duration;

use houyicoder_api::agent_event::AgentEventHandlers;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_context::{
    CheckpointManifest, MemoryEntry, MemoryError, MemoryScope, MemorySummary, SessionId,
    SessionLogEntry,
};

pub use gates::{MemoryGateState, MemoryGates};
pub(crate) use mutation_log::MutationLog;
pub(crate) use preservation::{preserve_folded_context, preserve_session};

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

/// Coordinates memory state and lifecycle operations.
pub struct MemoryRuntime {
    store: Arc<dyn SessionLog>,
    provider: Option<Arc<dyn MemoryProvider>>,
    gates: MemoryGates,
    background: BackgroundMemory,
}

impl MemoryRuntime {
    /// Construct an enabled runtime without a provider or background workers.
    pub fn new(store: Arc<dyn SessionLog>) -> Self {
        Self {
            store,
            provider: None,
            gates: MemoryGates::new(true, true),
            background: BackgroundMemory::none(),
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
            gates,
            background: BackgroundMemory { extractor, dream },
        }
    }

    /// Return the configured provider.
    pub(crate) fn provider(&self) -> Option<&Arc<dyn MemoryProvider>> {
        self.provider.as_ref()
    }

    /// Install a provider during crate-internal incremental assembly.
    pub(crate) fn install_provider(&mut self, provider: Arc<dyn MemoryProvider>) {
        self.provider = Some(provider);
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
    /// auto_memory is off, no provider is configured, or recall returns nothing.
    pub(crate) async fn recall(&self, session: SessionId) -> Result<(), crate::agent::RunError> {
        recall::recall(&self.store, self.provider.as_ref(), &self.gates, session).await
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

    /// Format the memory index for the system prompt prefix. Returns None
    /// when no provider is configured or the store is empty. Capped at 200
    /// entries.
    pub(crate) fn format_index(&self) -> Option<String> {
        let memory = self.provider.as_ref()?;
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
    pub(crate) async fn fire_background<F>(&self, session: SessionId, reward: F)
    where
        F: FnOnce() -> RewardSnapshot,
    {
        if std::env::var("HOUYICODER_REWARD_OFF").is_ok() {
            return;
        }
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
            dream.execute_dream(Some(reward()), Some(&session.to_string()));
        }
    }

    /// Await in-flight dream tasks until they finish or the timeout expires.
    pub(crate) async fn join_background(&self, timeout: Duration) {
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
    }

    /// The dream's cross-session scan root, or None when in-memory.
    pub(crate) fn dream_session_log_root(&self) -> Option<&std::path::Path> {
        self.background
            .dream
            .as_ref()
            .and_then(|d| d.session_log_root.as_deref())
    }
}
