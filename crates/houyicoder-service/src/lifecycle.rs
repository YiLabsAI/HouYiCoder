//! Session lifecycle: the durable session record, the live-runner store, and the
//! transition verbs. A session is the isolation and deployment unit; its event
//! append and replay contract lives one layer down in storage.
//!
//! A SessionRecord carries the event log cursor, any pending permission turn
//! (indexed by session, so a reconnect re-sends it to the reattaching client),
//! the runner checkpoint, and the control lease holder. An optional registry
//! persists records across processes; re-send and resume await engine support.

pub mod prompt_backfill;
pub mod prune;
pub mod reclaim;
pub mod repair;

mod record;
mod registry;
mod state;
mod store;
mod transitions;

#[cfg(test)]
mod tests;

use houyicoder_async::PFut;
use houyicoder_context::{AgentId, SessionId};

pub use record::{PendingPermission, PendingTurn, SessionRecord};
pub use registry::{FileSessionRegistry, SessionRegistry};
pub use state::{LifecycleError, LifecycleState};
pub use store::SessionLeaseStore;

/// The session lifecycle contract. Object-safe (PFut) so the composition root
/// holds a single dispatcher and the runtime swaps behind it. This file lands
/// the signatures; the state-machine runtime that enforces transitions lands in
/// a later runtime step.
///
/// Verbs reuse established session verbs (load, take_control, detach, handoff,
/// cancel) so no new client-facing state is invented; the control-lease timing
/// (take_control waits for the prompt to end by default, force cancels the
/// in-flight run and takes) is a runtime concern, not a trait-shape concern.
pub trait Lifecycle: Send + Sync {
    /// Reconnect to a session by id. On reconnect the lease auto-takes (or the
    /// caller becomes an observer), and any session-indexed pending permission
    /// is re-sent so the reattaching client can answer it. The returned record
    /// reflects the post-reconnect state (PendingPermission when a pending ask
    /// was retained, Running otherwise).
    fn load_session(
        &self,
        session_id: SessionId,
    ) -> PFut<'_, Result<SessionRecord, LifecycleError>>;

    /// Take exclusive control of a session. Default waits for the in-flight
    /// prompt to end; force cancels the run and takes immediately. Fails closed
    /// when another client holds the lease and force is false.
    fn take_control(
        &self,
        session_id: SessionId,
        force: bool,
    ) -> PFut<'_, Result<(), LifecycleError>>;

    /// Detach a client from a session. The session survives; any pending
    /// permission ask is retained (session-indexed) for the next reattaching
    /// client.
    fn detach(&self, session_id: SessionId) -> PFut<'_, Result<(), LifecycleError>>;

    /// Hand off the session to a target agent. The current session transitions
    /// to Shutdown (terminal); spawning the target is a NEW SessionRecord's
    /// Startup, orchestrated by the composition root through the spawn
    /// chokepoint. This method marks the handoff; the spawn itself is the
    /// caller's responsibility so the lifecycle layer does not couple to the
    /// spawn trait.
    fn handoff(
        &self,
        session_id: SessionId,
        target: AgentId,
    ) -> PFut<'_, Result<(), LifecycleError>>;

    /// Abort a session. Reaps any in-flight run and any pending permission ask,
    /// transitioning the session to Cancelled.
    fn cancel(&self, session_id: SessionId) -> PFut<'_, Result<(), LifecycleError>>;

    /// Read the current state of a session (a best-effort snapshot the runtime
    /// exposes; callers must not treat it as authoritative across an async
    /// boundary).
    fn state(&self, session_id: SessionId) -> LifecycleState;
}
