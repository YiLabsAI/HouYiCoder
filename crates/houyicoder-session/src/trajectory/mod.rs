//! The session's in-process trajectory mirror.
//!
//! The raw append-only log is the source of truth; this is the sync view of it
//! a reader can hold without touching the disk. One session's mirror keeps its
//! finalized events, the durable revision they add up to, the epoch the current
//! history began in, and the whole-session summary folded from them — all under
//! one lock, so a reader cannot see figures from a different revision than the
//! events they came from.
//!
//! Appending writes here; reading never does. The store calls the note methods
//! on its append path and the read methods on its read path, so the ordering
//! that keeps the two consistent lives in one place instead of being spread
//! through the facade. What each event contributes to the session's figures is
//! the summary module's business.

mod summary;

use std::collections::HashMap;
use std::sync::Mutex;

use houyicoder_api::session::{TrajectoryHead, TrajectoryRevision, last_user_input_id};
use houyicoder_context::{EventId, SessionId, SessionLogEntry};

use summary::TrajectorySummaryState;

/// One session's durable facts, mirrored in memory.
#[derive(Default)]
struct SessionMirror {
    events: Vec<SessionLogEntry>,
    /// Durable events in the mirror. Tracked as events are taken rather than
    /// counted on read, which would walk the vector on every call.
    durable_events: usize,
    last_durable_id: Option<EventId>,
    /// The first durable event of the mirror, which is what makes the epoch
    /// identity: a clear resets the mirror, so its next event begins a new one.
    epoch_event_id: Option<EventId>,
    summary: TrajectorySummaryState,
}

impl SessionMirror {
    fn head(&self) -> TrajectoryHead {
        TrajectoryHead {
            revision: TrajectoryRevision {
                event_count: self.events.len(),
                last_event_id: self.events.last().map(|event| event.id),
                durable_event_count: self.durable_events,
                last_durable_event_id: self.last_durable_id,
                epoch_event_id: self.epoch_event_id,
            },
            summary: self.summary.snapshot(),
        }
    }
}

/// Every session's mirror, under the one lock that keeps events and figures
/// moving together.
#[derive(Default)]
pub(super) struct TrajectoryMirrors {
    inner: Mutex<HashMap<SessionId, SessionMirror>>,
}

impl TrajectoryMirrors {
    /// Note a streaming delta: it advances the span and joins the event list,
    /// and moves no durable counter because it is not a durable fact.
    pub(super) fn note_delta(&self, session: SessionId, event: SessionLogEntry) {
        let mut mirrors = self.lock();
        let mirror = mirrors.entry(session).or_default();
        mirror.summary.record(&event);
        mirror.events.push(event);
    }

    /// Note a durable event: the summary folds it, the revision moves, and the
    /// first durable event of a mirror is the epoch its history began in.
    pub(super) fn note_durable(&self, session: SessionId, event: SessionLogEntry) {
        let mut mirrors = self.lock();
        let mirror = mirrors.entry(session).or_default();
        mirror.summary.record(&event);
        if mirror.durable_events == 0 {
            mirror.epoch_event_id = Some(event.id);
        }
        mirror.durable_events += 1;
        mirror.last_durable_id = Some(event.id);
        mirror.events.push(event);
    }

    /// Replace a session's mirror with a history read from disk, folding its
    /// summary in the same pass so a resume does not read the log once per
    /// figure. An empty history leaves the session with no mirror.
    pub(super) fn replace(&self, session: SessionId, events: Vec<SessionLogEntry>) {
        if events.is_empty() {
            self.reset(session);
            return;
        }
        let mut summary = TrajectorySummaryState::default();
        for event in &events {
            summary.record(event);
        }
        let mirror = SessionMirror {
            durable_events: events.len(),
            last_durable_id: events.last().map(|event| event.id),
            epoch_event_id: events.first().map(|event| event.id),
            summary,
            events,
        };
        self.lock().insert(session, mirror);
    }

    /// The finalized events in append order.
    pub(super) fn snapshot(&self, session: SessionId) -> Vec<SessionLogEntry> {
        self.lock()
            .get(&session)
            .map(|mirror| mirror.events.clone())
            .unwrap_or_default()
    }

    /// The id of the latest mirrored event, or None when the session has no
    /// mirror entries. Reads the tail in place — no clone of the log.
    pub(super) fn last_id(&self, session: SessionId) -> Option<EventId> {
        self.lock()
            .get(&session)
            .and_then(|mirror| mirror.events.last())
            .map(|event| event.id)
    }

    /// The id of the latest durable user input, or None when the session holds
    /// none yet. Scans the mirrored events under the lock, so it costs no clone
    /// of the log.
    pub(super) fn last_user_input_id(&self, session: SessionId) -> Option<EventId> {
        self.lock()
            .get(&session)
            .and_then(|mirror| last_user_input_id(&mirror.events))
    }

    /// Only the finalized suffix beginning at start.
    pub(super) fn since(&self, session: SessionId, start: usize) -> Vec<SessionLogEntry> {
        self.lock()
            .get(&session)
            .map(|mirror| mirror.events.get(start..).unwrap_or_default().to_vec())
            .unwrap_or_default()
    }

    /// The session's trajectory revision and whole-session summary under one
    /// lock, so the two describe the same revision.
    pub(super) fn head(&self, session: SessionId) -> TrajectoryHead {
        self.lock()
            .get(&session)
            .map(SessionMirror::head)
            .unwrap_or_default()
    }

    /// Drop a session's mirror, leaving the append-only log untouched.
    pub(super) fn reset(&self, session: SessionId) {
        self.lock().remove(&session);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, SessionMirror>> {
        self.inner.lock().expect("session mirrors mutex poisoned")
    }
}
