//! The in-memory session store: record and state maps, the lease flip, and
//! the parked-turn bookkeeping.
//!
//! The store owns storage and the atomic mutations over it. The transition
//! verbs themselves live in the sibling transition module; both halves read the
//! same maps, so the fields are visible to it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use houyicoder_context::{EventId, SessionId};
use houyicoder_protocol::frontend::run::ApprovalDecision;

use super::record::{PendingTurn, SessionRecord};
use super::registry::SessionRegistry;
use super::state::{LifecycleError, LifecycleState};

/// An in-memory SessionRecord + state store: the first concrete Lifecycle.
/// Tracks lease holders, pending permission asks (session-indexed, not
/// connection-indexed, so a detach + reconnect re-sends the pending ask),
/// and the lifecycle state per session. The adapter calls into this for
/// load/takeControl/detach/cancel; the runner abort (force cancel) stays in
/// the adapter, which owns the Arc<Runner> — the store only tracks state.
/// Arc-backed so the composition root shares one store between the adapter,
/// the runner host, and the IO bridge without each copying the maps.
#[derive(Clone)]
pub struct SessionLeaseStore {
    pub(super) inner: Arc<Inner>,
}

pub(super) struct Inner {
    pub(super) sessions: Mutex<HashMap<SessionId, SessionRecord>>,
    pub(super) states: Mutex<HashMap<SessionId, LifecycleState>>,
    /// Optional persistence backing. None keeps the in-memory fast path
    /// (single-process). When present, mutations persist so a new process
    /// that never held the session in memory can load it on reconnect.
    pub(super) registry: Option<Arc<dyn SessionRegistry>>,
}

impl SessionLeaseStore {
    /// In-memory only. The single-process fast path: no persistence, so a
    /// new process cannot reload sessions. Use with_registry to survive
    /// across processes.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                sessions: Mutex::new(HashMap::new()),
                states: Mutex::new(HashMap::new()),
                registry: None,
            }),
        }
    }

    /// Back the store with a registry. Insertions and state mutations persist;
    /// load_session hydrates from the registry when the session is not in
    /// memory (the cross-process reconnect path).
    pub fn with_registry(registry: Arc<dyn SessionRegistry>) -> Self {
        Self {
            inner: Arc::new(Inner {
                sessions: Mutex::new(HashMap::new()),
                states: Mutex::new(HashMap::new()),
                registry: Some(registry),
            }),
        }
    }

    /// Register a fresh session record + mark it Running. The composition
    /// root calls this when it spawns a runner for a new session. When a
    /// registry is attached, the record is persisted.
    pub fn insert(&self, record: SessionRecord) {
        let sid = record.session_id;
        self.inner
            .states
            .lock()
            .expect("state lock")
            .insert(sid, LifecycleState::Running);
        if let Some(reg) = &self.inner.registry {
            // Persistence is best-effort at the insert seam: a write failure
            // does not block session start (the in-memory record is the
            // source of truth for this process). The registry reports the
            // failure via the Io error on the next load, not here.
            drop(reg.register(&record));
        }
        self.inner
            .sessions
            .lock()
            .expect("session lock")
            .insert(sid, record);
    }

    /// Persist the current in-memory record + state when a registry is
    /// attached. Centralizes the writeback so each mutating verb stays a
    /// one-liner.
    pub(super) fn persist(&self, session_id: SessionId) {
        let Some(reg) = &self.inner.registry else {
            return;
        };
        let record = self
            .inner
            .sessions
            .lock()
            .expect("session lock")
            .get(&session_id)
            .cloned();
        let Some(record) = record else {
            // The session was removed (handoff) — drop the persisted file so
            // a reconnecting client sees NotFound, matching in-memory state.
            drop(reg.remove(session_id));
            return;
        };
        drop(reg.register(&record));
    }

    /// Upsert the parked PendingTurn for a session. Creates a minimal record
    /// if none exists (the in-process host does not always insert one before
    /// a run lands an interruption). The serve run path calls this on
    /// Interruption resolve (before emitting the first ask) and clears it
    /// (None) once all verdicts are in and the run resumes.
    pub(crate) fn set_pending(&self, session: SessionId, pending: Option<PendingTurn>) {
        let mut sessions = self.inner.sessions.lock().expect("session lock");
        let record = sessions.entry(session).or_insert_with(|| SessionRecord {
            session_id: session,
            event_cursor: EventId::new(),
            pending: None,
            runner_checkpoint: Vec::new(),
            lease_holder: None,
        });
        record.pending = pending;
        drop(sessions);
        self.persist(session);
    }

    /// Read a clone of the parked PendingTurn. The serve-start re-emit path
    /// reads this on a reattaching connection so it can re-send the remaining
    /// asks + feed the prior verdicts to runner.resume. None when no turn is
    /// parked (the single-shot host=None path never writes one).
    pub(crate) fn pending(&self, session: SessionId) -> Option<PendingTurn> {
        self.inner
            .sessions
            .lock()
            .expect("session lock")
            .get(&session)
            .and_then(|r| r.pending.clone())
    }

    /// Move the head unanswered ask into the decided list — one verdict
    /// arrived. A mid-batch disconnect after this leaves the remaining tail
    /// (the unsent + unanswered asks) plus the decided verdicts, so the
    /// reattaching client re-receives exactly the unanswered ones and the
    /// runner resumes with the full decided set. No-op when no turn is parked.
    pub(crate) fn advance_pending(&self, session: SessionId, decision: ApprovalDecision) {
        let mut sessions = self.inner.sessions.lock().expect("session lock");
        if let Some(record) = sessions.get_mut(&session)
            && let Some(turn) = &mut record.pending
        {
            if !turn.remaining.is_empty() {
                turn.remaining.remove(0);
            }
            turn.decided.push(decision);
        }
        drop(sessions);
        self.persist(session);
    }

    /// Flip the lifecycle state for a session. The in-process host path uses
    /// this to take the lease (mark Running when a serve starts) and release
    /// it (mark Detached when the serve returns, retaining any parked
    /// PendingTurn). The full take_control / detach / cancel verbs remain
    /// for the cross-process + ACP paths; this is the minimal state flip the
    /// reconnect-replay host needs for its occupied-guard.
    pub(crate) fn set_state(&self, session: SessionId, state: LifecycleState) {
        self.inner
            .states
            .lock()
            .expect("state lock")
            .insert(session, state);
        self.persist(session);
    }

    /// Atomically check the session state and take the lease if it is free.
    /// Returns Ok(()) if the lease was taken (prior state was Detached or
    /// PendingPermission — set to Running under the same lock). Returns Err if
    /// the session is terminal (Cancelled/Shutdown) or already Running. This
    /// closes the check-then-set race where two concurrent serve_session calls
    /// could both observe Detached and both proceed to set Running.
    pub(crate) fn try_take_lease(&self, session: SessionId) -> Result<(), LifecycleError> {
        let mut states = self.inner.states.lock().expect("state lock");
        match states.get(&session).copied() {
            Some(LifecycleState::Running) => {
                Err(LifecycleError::LeaseHeld("another connection".into()))
            }
            Some(LifecycleState::Cancelled | LifecycleState::Shutdown) => {
                Err(LifecycleError::NotFound)
            }
            _ => {
                states.insert(session, LifecycleState::Running);
                drop(states);
                self.persist(session);
                Ok(())
            }
        }
    }
}

impl Default for SessionLeaseStore {
    fn default() -> Self {
        Self::new()
    }
}
