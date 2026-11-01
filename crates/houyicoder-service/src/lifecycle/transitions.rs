//! The lifecycle verbs over the session store: loading a stored record,
//! taking control, detaching, handing off, cancelling, and the state snapshot.
//!
//! Each verb states which states it accepts and what it does to the lease and
//! the parked turn. Terminal states are refused rather than revived, so a
//! reaped session stays reaped and a stale client sees the terminal record.

use houyicoder_async::PFut;
use houyicoder_context::{AgentId, SessionId};

use super::Lifecycle;
use super::record::SessionRecord;
use super::state::{LifecycleError, LifecycleState};
use super::store::SessionLeaseStore;

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
        _target: AgentId,
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
