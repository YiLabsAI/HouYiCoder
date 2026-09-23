//! The append-only session log port: the engine-facing contract for event
//! logging, replay, and cursor (checkpoint) support. Signatures reference
//! context types (SessionLogEntry, ContextSnapshot). The concrete facade (hash
//! chain, delta counter, trajectory mirror) lives in the session crate; the
//! engine depends on this trait so it does not depend on the session crate
//! directly.

use houyicoder_async::PFut;
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, ContextSnapshot, EventId,
    SessionEvent, SessionId, SessionLogEntry,
};
use houyicoder_protocol::llm::Usage;

/// The delegated usage a session's children reported in durable returns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubagentUsage {
    /// Children that reached a terminal, whether or not they reported usage.
    pub calls: usize,
    /// Children that reached a terminal without reporting any usage.
    pub unmeasured_calls: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub reasoning_tokens: u64,
}

impl SubagentUsage {
    /// Fold one durable event into the projection. Events other than a child
    /// return leave it unchanged.
    pub fn record(&mut self, event: &SessionEvent) {
        let SessionEvent::SubagentReturn {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_tokens,
            ..
        } = event
        else {
            return;
        };
        self.calls = self.calls.saturating_add(1);
        if *input_tokens == 0
            && *output_tokens == 0
            && *cache_read_input_tokens == 0
            && *cache_write_input_tokens == 0
            && *reasoning_tokens == 0
        {
            self.unmeasured_calls = self.unmeasured_calls.saturating_add(1);
        }
        self.input_tokens = self.input_tokens.saturating_add(*input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(*output_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(*cache_read_input_tokens);
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .saturating_add(*cache_write_input_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(*reasoning_tokens);
    }

    /// Convert the projection to the provider usage shape used by status.
    pub fn to_usage(self) -> Usage {
        let clamp = |v: u64| v.min(u32::MAX as u64) as u32;
        let input_tokens = clamp(self.input_tokens);
        let output_tokens = clamp(self.output_tokens);
        Usage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens.saturating_add(output_tokens),
            non_cached_input_tokens: clamp(
                self.input_tokens
                    .saturating_sub(self.cache_read_input_tokens),
            ),
            cache_read_input_tokens: clamp(self.cache_read_input_tokens),
            cache_write_input_tokens: clamp(self.cache_write_input_tokens),
            reasoning_tokens: clamp(self.reasoning_tokens),
        }
    }
}

/// Sum the delegated usage in a durable event slice.
pub fn aggregate_subagent_usage(events: &[SessionLogEntry]) -> SubagentUsage {
    let mut total = SubagentUsage::default();
    for event in events {
        total.record(&event.event);
    }
    total
}

/// The engine-facing session log. Object-safe (PFut) so the engine holds
/// Arc<dyn SessionLog> and the concrete session facade swaps behind it. The
/// facade layers the hash chain, delta-persistence counter, and trajectory
/// mirror on top of a ContextBackend; this port surfaces only what the engine
/// drives.
pub trait SessionLog: Send + Sync {
    /// Append an event to the lossless log. The facade sets prev_hash; the
    /// backend stores it verbatim.
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>>;

    /// Read the full event log for a session in append order.
    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>>;

    /// Assemble the context: the full replay plus the latest
    /// checkpoint manifest, so the caller can apply the disposition plan.
    fn current_view(&self, session: SessionId) -> PFut<'_, Result<ContextSnapshot, ContextError>>;

    /// The finalized events in append order (sync, in-memory mirror). Empty
    /// until events are appended this process for the session.
    fn trajectory_snapshot(&self, session: SessionId) -> Vec<SessionLogEntry>;

    /// The id of the latest mirrored event, or None when the session has no
    /// mirror yet. Defaults to reading the snapshot; a store with an indexed
    /// mirror overrides it to answer without cloning the log.
    fn last_trajectory_id(&self, session: SessionId) -> Option<EventId> {
        self.trajectory_snapshot(session).pop().map(|e| e.id)
    }

    /// Clone the finalized suffix beginning at start from the in-memory mirror.
    /// Implementations should avoid cloning the already-consumed prefix.
    fn trajectory_since(&self, session: SessionId, start: usize) -> Vec<SessionLogEntry> {
        self.trajectory_snapshot(session)
            .into_iter()
            .skip(start)
            .collect()
    }

    /// The delegated usage folded from the durable returns in the session's
    /// current view. Required rather than defaulted: a status poll reads this
    /// every second, so an implementation that silently inherited a scan of
    /// the whole log would turn a constant-time read into linear work.
    fn subagent_usage(&self, session: SessionId) -> SubagentUsage;

    /// Drop the in-memory trajectory mirror for a session. The backend log
    /// is untouched.
    fn reset_trajectory(&self, session: SessionId);

    /// Persist a compaction manifest (checkpoint).
    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>>;

    /// Read a checkpoint manifest by id.
    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>>;

    /// List checkpoint ids for a session, oldest first.
    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>>;

    /// Borrow the underlying backend for CAS operations (block_put /
    /// block_get) from the projection layer without owning the store.
    fn backend(&self) -> &dyn ContextBackend;

    /// The durable sessions root this log's backend persists under, for
    /// cross-session readers (the dream's retry scan). Derived from the
    /// backend, not configured, so a reader can never disagree with the
    /// writer about the root. None on a backend that is not disk-backed -
    /// an in-memory build carries no cross-session history and must not
    /// read the real home.
    fn session_log_root(&self) -> Option<std::path::PathBuf> {
        None
    }
}
