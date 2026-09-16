//! Run lifecycle state machine: the single source of truth for whether a
//! run is in flight, paused for approval, or cancelling.

use std::time::Instant;

use houyicoder_protocol::envelope::RequestId;

/// The run lifecycle. Idle means no run; the other variants carry the
/// ActiveRun so the request id, start time, and progress live in one place.
/// Transitions are gated: a Waiting run does not accept a second submit, and
/// a terminal outcome (Done, Error, loss) returns to Idle exactly once.
#[derive(Debug)]
pub enum RunState {
    Idle,
    /// A run is in flight: frames may stream, the spinner is active.
    Running(ActiveRun),
    /// A run is paused for a reverse permission or trust ask. The run keeps
    /// its identity and start time; the verdict resumes Running without
    /// resetting the clock.
    Waiting(ActiveRun),
    /// The user cancelled; the run resolves Interrupted on its original id.
    Cancelling(ActiveRun),
}

/// One in-flight run: the request id the server correlates, the wall-clock
/// start, and the streaming progress. Started at the moment the enqueue
/// succeeded locally — the run is "running" from the App's perspective
/// even before the first frame arrives.
#[derive(Debug)]
pub struct ActiveRun {
    pub request: RequestId,
    pub started_at: Instant,
}

impl RunState {
    /// True when a run is in flight in any non-Idle variant.
    pub fn is_active(&self) -> bool {
        !matches!(self, RunState::Idle)
    }

    /// True only in the Cancelling variant.
    pub fn is_cancelling(&self) -> bool {
        matches!(self, RunState::Cancelling(_))
    }

    /// The wall-clock start of the active run, or None when Idle.
    pub fn started_at(&self) -> Option<Instant> {
        match self {
            RunState::Running(r) | RunState::Waiting(r) | RunState::Cancelling(r) => {
                Some(r.started_at)
            }
            RunState::Idle => None,
        }
    }

    /// The request id of the active run, or None when Idle.
    pub fn request_id(&self) -> Option<RequestId> {
        match self {
            RunState::Running(r) | RunState::Waiting(r) | RunState::Cancelling(r) => {
                Some(r.request)
            }
            RunState::Idle => None,
        }
    }

    /// Idle → Running. The caller passes the issued request id and the
    /// wall-clock start.
    pub fn start(&mut self, request: RequestId, now: Instant) {
        *self = RunState::Running(ActiveRun {
            request,
            started_at: now,
        });
    }

    /// Running → Waiting. No-op if not Running (a late permission ask after
    /// cancel or completion must not revive a dead run).
    pub fn begin_waiting(&mut self) {
        if let RunState::Running(run) = self {
            let moved = ActiveRun {
                request: run.request,
                started_at: run.started_at,
            };
            *self = RunState::Waiting(moved);
        }
    }

    /// Waiting → Running. No-op if not Waiting (a late verdict after the run
    /// ended must not flip a dead run back to Running).
    pub fn end_waiting(&mut self) {
        if let RunState::Waiting(run) = self {
            let moved = ActiveRun {
                request: run.request,
                started_at: run.started_at,
            };
            *self = RunState::Running(moved);
        }
    }

    /// Running/Waiting → Cancelling. No-op if Idle.
    pub fn begin_cancel(&mut self) {
        match self {
            RunState::Running(run) | RunState::Waiting(run) => {
                let moved = ActiveRun {
                    request: run.request,
                    started_at: run.started_at,
                };
                *self = RunState::Cancelling(moved);
            }
            _ => {}
        }
    }

    /// Any active → Idle. Returns the ActiveRun if one was in flight, so the
    /// caller can run completion processing. Called on Done, Error, or
    /// connection loss.
    pub fn finish(&mut self) -> Option<ActiveRun> {
        match std::mem::replace(self, RunState::Idle) {
            RunState::Idle => None,
            RunState::Running(run) | RunState::Waiting(run) | RunState::Cancelling(run) => {
                Some(run)
            }
        }
    }
}
