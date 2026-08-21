//! The append-only session log port: the engine-facing contract for event
//! logging, replay, and cursor (checkpoint) support. Signatures reference
//! context types (TurnEvent, ContextSnapshot). The concrete facade (hash
//! chain, delta counter, trajectory mirror) lives in the session crate; the
//! engine depends on this trait so it does not depend on the session crate
//! directly.

use houyicoder_async::PFut;
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, ContextSnapshot, EventId,
    SessionId, TurnEvent,
};

/// The engine-facing session log. Object-safe (PFut) so the engine holds
/// Arc<dyn SessionLog> and the concrete session facade swaps behind it. The
/// facade layers the hash chain, delta-persistence counter, and trajectory
/// mirror on top of a ContextBackend; this port surfaces only what the engine
/// drives.
pub trait SessionLog: Send + Sync {
    /// Append an event to the lossless log. The facade sets prev_hash; the
    /// backend stores it verbatim.
    fn append(&self, event: TurnEvent) -> PFut<'_, Result<EventId, ContextError>>;

    /// Read the full event log for a session in append order.
    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<TurnEvent>, ContextError>>;

    /// Assemble the served context view: the full replay plus the latest
    /// checkpoint manifest, so the caller can apply the disposition plan.
    fn current_view(&self, session: SessionId) -> PFut<'_, Result<ContextSnapshot, ContextError>>;

    /// The finalized events in append order (sync, in-memory mirror). Empty
    /// until events are appended this process for the session.
    fn trajectory_snapshot(&self, session: SessionId) -> Vec<TurnEvent>;

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
