//! Lifecycle management: session records, the live-runner host registry, and
//! the lifecycle state machine. The trait, the SessionRecord shape, and the
//! state enum land here; the runtime that drives the higher-level transitions
//! (pending-ask re-send on reconnect, runner-resume-from-checkpoint across a
//! process boundary) is engine-level and deferred.
//!
//! Session is the isolation + deployment unit (the docker-style layering seam).
//! A SessionRecord is the durable, session-indexed companion to a live runner:
//! it carries the event log cursor, any pending permission ask (indexed by
//! session, not by connection, so a detach + reconnect re-sends the pending ask
//! to whichever client reattaches), the runner checkpoint, and the current
//! control lease holder. The trait stays here in the composition layer (the one
//! that owns session records + hosts live runners); the event append/replay
//! contract (SessionLog) lives one layer down in ports as the pure storage
//! primitive the engine consumes.
//!
//! Persistence seam: an optional SessionRegistry backs the in-memory store so
//! records survive across processes. Single-process callers keep the in-memory
//! fast path; the registry is the persistence seam a reconnecting client loads
//! from when its new process never held the session in memory. The
//! re-send-on-reconnect path (re-sending a pending permission ask to a
//! reattaching client over the wire) and the runner-resume-from-checkpoint
//! (re-resuming a run paused at an interruption across a process boundary)
//! both need engine support and are deferred. The pure-store state-machine
//! enforcement here is the foundation both build on.

use houyicoder_async::PFut;
use houyicoder_context::{EventId, SessionId};
use houyicoder_protocol::frontend::run::ApprovalDecision;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// The durable, session-indexed companion to a live runner. Fields are pub so
/// the runtime layer reads and updates them; construction goes through the
/// composition root, never external crates.
///
/// pending is indexed by session, not by connection: a client that detaches
/// while a permission ask is in flight leaves the ask here, and whichever client
/// reattaches via load_session receives the re-sent ask. This is the single
/// source of truth for pending-permission state, so the lifecycle layer and the
/// control-lease spec never hold two divergent copies.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRecord {
    /// The session this record tracks.
    pub session_id: SessionId,
    /// The last event id appended to the session log (the resume cursor).
    pub event_cursor: EventId,
    /// The half-live turn retained across detach + reconnect: the unanswered
    /// asks plus the verdicts already received. None when no turn is parked at
    /// an interruption. Indexed by session (not connection) so a reattaching
    /// client re-receives the remaining asks and the runner resumes with the
    /// full decided set. Carries the whole turn, not a single ask, so a
    /// mid-batch disconnect does not lose the verdicts already given (which
    /// would otherwise die with the connection-local Vec and could not be
    /// re-asked without double-appending the audit chain).
    pub pending: Option<PendingTurn>,
    /// An opaque runner checkpoint the runtime can resume from (the serialized
    /// runner state at the last safe resume point). Owned bytes so the lifecycle
    /// layer does not depend on a runner-internal type.
    pub runner_checkpoint: Vec<u8>,
    /// The current control lease holder (a client or connection identifier), or
    /// None when the session is detached with no live holder.
    pub lease_holder: Option<String>,
}

/// A pending permission ask retained across detach + reconnect. Indexed by the
/// session, not by the connection that originated it, so the re-sent ask reaches
/// whichever client reattaches. Matches the reverse-request payload so the
/// runtime can re-send it verbatim.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingPermission {
    /// The provider-minted tool-call id the verdict answers.
    pub call_id: String,
    /// The tool name the ask is about.
    pub tool: String,
    /// The tool input the ask carries (may be edited by the human).
    pub input: Value,
}

/// The full half-live turn parked at an interruption: the asks still awaiting a
/// verdict (in the order the server emits them, head first) plus the verdicts
/// already received. A disconnect mid-batch keeps both — the remaining asks are
/// re-emitted on reattach, and the runner resumes with the full decided set
/// (already-given verdicts plus the ones the reattaching client supplies).
///
/// Both fields are wire types so the record stays the cross-process serde
/// boundary; the single-approval common case degenerates to a one-element
/// remaining list and an empty decided list.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingTurn {
    /// Unanswered asks still to emit (or re-emit on reconnect), head first.
    pub remaining: Vec<PendingPermission>,
    /// Verdicts already received this turn, in the order received. Fed to
    /// runner.resume together with the freshly-received ones once remaining
    /// drains.
    pub decided: Vec<ApprovalDecision>,
}

/// The lifecycle states a session moves through. A closed, stable set: adding a
/// state is a deliberate breaking change (every runtime match arm must handle
/// it), so the enum is not non_exhaustive. Shutdown and Cancelled are terminal.
///
/// Transition shape (this trait documents it; a later runtime step enforces it):
/// Startup -> Running on init; Running <-> Checkpointed on compaction resume;
/// Running -> PendingPermission when a tool asks; PendingPermission -> Running
/// on reply or -> Cancelled on a reap, or -> Detached on disconnect (pending
/// retained); Detached -> PendingPermission on reconnect WITH pending (the ask
/// is re-sent, still awaiting reply) or -> Running without pending (lease
/// auto-take or observer); Running -> Cancelled on abort; Running -> Shutdown
/// on Handoff (the spawn target is a NEW SessionRecord's Startup, not an edge
/// into this machine) or on session end; Detached -> Shutdown on stop or lease
/// expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleState {
    Startup,
    Running,
    Checkpointed,
    PendingPermission,
    Detached,
    /// Terminal: the session handed off (spawn target is a new session) or ended.
    Shutdown,
    /// Terminal: the session was aborted (RunCancel or a pending-ask reap).
    Cancelled,
}

/// Failures a lifecycle operation can report.
#[derive(Debug)]
pub enum LifecycleError {
    /// No session matched the id.
    NotFound,
    /// The session is not in a state the operation allows.
    InvalidTransition(String),
    /// Another client holds the control lease and force was not requested.
    LeaseHeld(String),
    /// An underlying store or I/O failure.
    Io,
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("session not found"),
            Self::InvalidTransition(msg) => write!(f, "invalid lifecycle transition: {msg}"),
            Self::LeaseHeld(msg) => write!(f, "control lease held by another client: {msg}"),
            Self::Io => f.write_str("lifecycle store I/O failure"),
        }
    }
}

impl std::error::Error for LifecycleError {}

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
        target: houyicoder_context::AgentId,
    ) -> PFut<'_, Result<(), LifecycleError>>;

    /// Abort a session. Reaps any in-flight run and any pending permission ask,
    /// transitioning the session to Cancelled.
    fn cancel(&self, session_id: SessionId) -> PFut<'_, Result<(), LifecycleError>>;

    /// Read the current state of a session (a best-effort snapshot the runtime
    /// exposes; callers must not treat it as authoritative across an async
    /// boundary).
    fn state(&self, session_id: SessionId) -> LifecycleState;
}

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
    inner: std::sync::Arc<Inner>,
}

struct Inner {
    sessions: std::sync::Mutex<std::collections::HashMap<SessionId, SessionRecord>>,
    states: std::sync::Mutex<std::collections::HashMap<SessionId, LifecycleState>>,
    /// Optional persistence backing. None keeps the in-memory fast path
    /// (single-process). When present, mutations persist so a new process
    /// that never held the session in memory can load it on reconnect.
    registry: Option<std::sync::Arc<dyn SessionRegistry>>,
}

impl SessionLeaseStore {
    /// In-memory only. The single-process fast path: no persistence, so a
    /// new process cannot reload sessions. Use with_registry to survive
    /// across processes.
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(Inner {
                sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
                states: std::sync::Mutex::new(std::collections::HashMap::new()),
                registry: None,
            }),
        }
    }

    /// Back the store with a registry. Insertions and state mutations persist;
    /// load_session hydrates from the registry when the session is not in
    /// memory (the cross-process reconnect path).
    pub fn with_registry(registry: std::sync::Arc<dyn SessionRegistry>) -> Self {
        Self {
            inner: std::sync::Arc::new(Inner {
                sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
                states: std::sync::Mutex::new(std::collections::HashMap::new()),
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
    fn persist(&self, session_id: SessionId) {
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

impl Lifecycle for SessionLeaseStore {
    fn load_session(
        &self,
        session_id: SessionId,
    ) -> PFut<'_, Result<SessionRecord, LifecycleError>> {
        Box::pin(async move {
            // Cross-process reconnect path: when the session is not in memory
            // and a registry is attached, hydrate from the persisted record.
            // The hydrated record is the post-detach snapshot (state Detached,
            // lease_holder None), so the reattach logic below applies as if
            // the client had disconnected in-process.
            let in_memory = self
                .inner
                .sessions
                .lock()
                .expect("session lock")
                .contains_key(&session_id);
            if !in_memory
                && let Some(reg) = &self.inner.registry
                && let Some(record) = reg.load(session_id)?
            {
                self.inner
                    .sessions
                    .lock()
                    .expect("session lock")
                    .insert(session_id, record);
                // A persisted record was detached when its process
                // exited; the reattach path below takes the lease
                // and moves the state to Running or PendingPermission.
                self.inner
                    .states
                    .lock()
                    .expect("state lock")
                    .insert(session_id, LifecycleState::Detached);
            }
            let mut sessions = self.inner.sessions.lock().expect("session lock");
            let record = sessions
                .get_mut(&session_id)
                .ok_or(LifecycleError::NotFound)?;
            let state = self
                .inner
                .states
                .lock()
                .expect("state lock")
                .get(&session_id)
                .copied()
                .unwrap_or(LifecycleState::Startup);
            // Terminal sessions never revive: a Cancelled or Shutdown session
            // returns its record as-is (no lease auto-take, no state change)
            // so a caller can inspect the reaped record. The wire layer maps
            // this to a session-gone signal; the store does not revive it.
            if matches!(state, LifecycleState::Cancelled | LifecycleState::Shutdown) {
                let snapshot = record.clone();
                drop(sessions);
                return Ok(snapshot);
            }
            // Lease auto-takes on reconnect when free; a held lease makes the
            // reattaching client an observer (it reads the record but does not
            // hold the lease). The store does not distinguish observer vs
            // holder beyond the lease_holder field — the adapter routes asks
            // to the holder only.
            if record.lease_holder.is_none() {
                record.lease_holder = Some("reattach".into());
                let next = if record.pending.is_some() {
                    LifecycleState::PendingPermission
                } else {
                    LifecycleState::Running
                };
                self.inner
                    .states
                    .lock()
                    .expect("state lock")
                    .insert(session_id, next);
            }
            let snapshot = record.clone();
            drop(sessions);
            self.persist(session_id);
            Ok(snapshot)
        })
    }

    fn take_control(
        &self,
        session_id: SessionId,
        force: bool,
    ) -> PFut<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            let mut sessions = self.inner.sessions.lock().expect("session lock");
            let record = sessions
                .get_mut(&session_id)
                .ok_or(LifecycleError::NotFound)?;
            let state = self
                .inner
                .states
                .lock()
                .expect("state lock")
                .get(&session_id)
                .copied()
                .unwrap_or(LifecycleState::Startup);
            if matches!(state, LifecycleState::Cancelled | LifecycleState::Shutdown) {
                return Err(LifecycleError::InvalidTransition(format!(
                    "cannot take control of a terminal session: {state:?}"
                )));
            }
            if let Some(holder) = record.lease_holder.as_ref()
                && !force
            {
                return Err(LifecycleError::LeaseHeld(holder.clone()));
            }
            record.lease_holder = Some("takeControl".into());
            if force {
                // Reap the pending ask (the adapter aborts the run); the
                // reaper is the adapter, the store only clears the record.
                record.pending = None;
            }
            drop(sessions);
            self.persist(session_id);
            Ok(())
        })
    }

    fn detach(&self, session_id: SessionId) -> PFut<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            let mut sessions = self.inner.sessions.lock().expect("session lock");
            let record = sessions
                .get_mut(&session_id)
                .ok_or(LifecycleError::NotFound)?;
            let state = self
                .inner
                .states
                .lock()
                .expect("state lock")
                .get(&session_id)
                .copied()
                .unwrap_or(LifecycleState::Startup);
            // A terminal session cannot detach (it has no live client to
            // disconnect). Detaching a detached session is a no-op return so
            // a double-detach from a buggy client does not fail.
            if matches!(state, LifecycleState::Cancelled | LifecycleState::Shutdown) {
                return Err(LifecycleError::InvalidTransition(format!(
                    "cannot detach a terminal session: {state:?}"
                )));
            }
            // Pending permission survives detach (session-indexed): the ask
            // is retained for the next reattaching client. Only the lease
            // and the state move.
            record.lease_holder = None;
            self.inner
                .states
                .lock()
                .expect("state lock")
                .insert(session_id, LifecycleState::Detached);
            drop(sessions);
            self.persist(session_id);
            Ok(())
        })
    }

    fn handoff(
        &self,
        session_id: SessionId,
        _target: houyicoder_context::AgentId,
    ) -> PFut<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            let state = self
                .inner
                .states
                .lock()
                .expect("state lock")
                .get(&session_id)
                .copied()
                .unwrap_or(LifecycleState::Startup);
            // A Cancelled session cannot hand off (it was aborted, not
            // migrated). An already-Shutdown session is a no-op (the
            // spawn target is a new session either way).
            if matches!(state, LifecycleState::Cancelled) {
                return Err(LifecycleError::InvalidTransition(
                    "cannot hand off a cancelled session".into(),
                ));
            }
            if matches!(state, LifecycleState::Shutdown) {
                return Ok(());
            }
            self.inner
                .sessions
                .lock()
                .expect("session lock")
                .remove(&session_id);
            self.inner
                .states
                .lock()
                .expect("state lock")
                .insert(session_id, LifecycleState::Shutdown);
            self.persist(session_id);
            Ok(())
        })
    }

    fn cancel(&self, session_id: SessionId) -> PFut<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            // Check state before the record lookup: a Shutdown session was
            // removed from the sessions map (handoff drops it), so the
            // record lookup would otherwise mask the terminal-state guard
            // with a NotFound. The state map is the authoritative terminal
            // signal.
            let state = self
                .inner
                .states
                .lock()
                .expect("state lock")
                .get(&session_id)
                .copied()
                .unwrap_or(LifecycleState::Startup);
            // Shutdown is terminal in a stronger sense (the session was
            // migrated or ended cleanly); reaping it as cancelled would
            // rewrite a clean exit as an abort. Cancel on an already-Cancelled
            // session is idempotent so a double-abort from the wire does not
            // surface a spurious error.
            if matches!(state, LifecycleState::Shutdown) {
                return Err(LifecycleError::InvalidTransition(
                    "cannot cancel a shutdown session".into(),
                ));
            }
            if matches!(state, LifecycleState::Cancelled) {
                return Ok(());
            }
            let mut sessions = self.inner.sessions.lock().expect("session lock");
            let record = sessions
                .get_mut(&session_id)
                .ok_or(LifecycleError::NotFound)?;
            record.pending = None;
            record.lease_holder = None;
            self.inner
                .states
                .lock()
                .expect("state lock")
                .insert(session_id, LifecycleState::Cancelled);
            drop(sessions);
            self.persist(session_id);
            Ok(())
        })
    }

    fn state(&self, session_id: SessionId) -> LifecycleState {
        self.inner
            .states
            .lock()
            .expect("state lock")
            .get(&session_id)
            .copied()
            .unwrap_or(LifecycleState::Startup)
    }
}

/// The persistence seam for session records. Object-safe so the store holds
/// a single shared dispatcher. The in-memory fast path skips this entirely;
/// an impl is attached when records must survive across processes.
///
/// The contract is single-writer-per-session: the composition root serializes
/// writes to a given session id (one live runner at a time owns a session).
/// A multi-writer impl needs an external lock and is deferred.
pub trait SessionRegistry: Send + Sync {
    /// Persist a record. An existing record for the same session id is
    /// overwritten atomically (a reader never sees a partial file).
    fn register(&self, record: &SessionRecord) -> Result<(), LifecycleError>;

    /// Read a record by session id. None when no record is persisted.
    fn load(&self, session_id: SessionId) -> Result<Option<SessionRecord>, LifecycleError>;

    /// Delete a record by session id. Missing is a no-op (not an error) so
    /// the handoff path can drop a file without a race-check.
    fn remove(&self, session_id: SessionId) -> Result<(), LifecycleError>;
}

/// A file-backed SessionRegistry: one JSON file per session id, named by the
/// ULID display string, under a root directory. Atomic write via tmp + rename
/// so a reconnecting reader never sees a half-written record.
///
/// V1 limitation: single-writer-per-session is assumed. Two processes writing
/// the same session id concurrently race and the last rename wins; a real
/// multi-writer needs a lock file or a single-owner server. The composition
/// root serializes writes today (one runner owns a session at a time), so the
/// assumption holds for v1.
#[derive(Clone)]
pub struct FileSessionRegistry {
    root: PathBuf,
}

impl FileSessionRegistry {
    /// Open a registry rooted at the given directory. The directory is
    /// created (recursively) if missing; existing records are readable as-is.
    pub fn open(root: PathBuf) -> Result<Self, LifecycleError> {
        std::fs::create_dir_all(&root).map_err(|_| LifecycleError::Io)?;
        Ok(Self { root })
    }

    fn path_for(&self, session_id: SessionId) -> PathBuf {
        self.root.join(format!("{}.json", session_id))
    }

    fn write_record(&self, record: &SessionRecord) -> Result<(), LifecycleError> {
        let path = self.path_for(record.session_id);
        let tmp = self.root.join(format!(".{}.tmp", record.session_id));
        let bytes = serde_json::to_vec(record).map_err(|_| LifecycleError::Io)?;
        std::fs::write(&tmp, bytes).map_err(|_| LifecycleError::Io)?;
        std::fs::rename(&tmp, &path).map_err(|_| LifecycleError::Io)?;
        Ok(())
    }
}

impl SessionRegistry for FileSessionRegistry {
    fn register(&self, record: &SessionRecord) -> Result<(), LifecycleError> {
        self.write_record(record)
    }

    fn load(&self, session_id: SessionId) -> Result<Option<SessionRecord>, LifecycleError> {
        let path = self.path_for(session_id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path).map_err(|_| LifecycleError::Io)?;
        let record: SessionRecord =
            serde_json::from_slice(&bytes).map_err(|_| LifecycleError::Io)?;
        Ok(Some(record))
    }

    fn remove(&self, session_id: SessionId) -> Result<(), LifecycleError> {
        let path = self.path_for(session_id);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|_| LifecycleError::Io)?;
        }
        Ok(())
    }
}

impl FileSessionRegistry {
    /// Drop every persisted record under the root. Test-only helper so a
    /// test suite can reset between cases without leaking records across
    /// runs. Production callers never need to wipe a registry this way
    /// (cancel + handoff already remove their files).
    #[cfg(test)]
    pub fn clear(&self) {
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for e in entries.flatten() {
                drop(std::fs::remove_file(e.path()));
            }
        }
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
