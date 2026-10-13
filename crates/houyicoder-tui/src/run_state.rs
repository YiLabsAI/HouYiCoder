//! Run lifecycle state machine: the single source of truth for whether a
//! run is in flight, paused for approval, or cancelling.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use houyicoder_protocol::envelope::RequestId;

use crate::state::BashProgress;
use crate::state::enums::LiveBlock;

/// Streaming progress for one in-flight run. Lives with the run, so the
/// transient preview and the tool runtime are dropped when the run finishes
/// rather than outliving it.
#[derive(Debug, Default)]
pub struct RunProgress {
    pub(crate) live_assistant_text: String,
    pub(crate) live_active: bool,
    pub(crate) live_reasoning_text: String,
    pub(crate) live_block: LiveBlock,
    pub(crate) thinking_started_at: Option<Instant>,
    pub(crate) last_delta_at: Option<Instant>,
    /// The per-call tool runtime: which calls are running and the bash
    /// ticker with the revision the slot cache keys on.
    pub(crate) tools: ToolRuntime,
}

/// The per-call tool runtime of one run: which calls are running, and for
/// the bash ones the elapsed and line count the chip shows.
///
/// One owner because the ticker and its revision have to move together. The
/// elapsed and line count are baked into the cached slot text, so a tick that
/// stored its value without moving the revision would leave the previous chip
/// on screen; and a tick only counts for a call the run still tracks, so an
/// out-of-order frame cannot resurrect a finished chip.
#[derive(Debug, Default)]
pub(crate) struct ToolRuntime {
    running: HashSet<String>,
    bash: HashMap<String, BashProgress>,
    revision: u64,
}

impl ToolRuntime {
    /// Whether any call is running. The spinner reads it to exempt a
    /// running tool from the stall gradient: a tool produces no token deltas
    /// but is not stuck.
    pub(crate) fn any_running(&self) -> bool {
        !self.running.is_empty()
    }

    /// Whether one call is running, for a test that has to name the call
    /// it tracked.
    #[cfg(test)]
    pub(crate) fn is_running(&self, call_id: &str) -> bool {
        self.running.contains(call_id)
    }

    /// Mark a call as running.
    pub(crate) fn begin(&mut self, call_id: String) {
        self.running.insert(call_id);
    }

    /// Drop a call and its ticker when its result lands. Returns whether the
    /// call was running, so the caller can tell a real completion from a
    /// duplicate result frame.
    pub(crate) fn finish(&mut self, call_id: &str) -> bool {
        let was_running = self.running.remove(call_id);
        if self.bash.remove(call_id).is_some() {
            self.revision = self.revision.wrapping_add(1);
        }
        was_running
    }

    /// Record a tick for a call that is still running. A tick for a call the
    /// run no longer tracks is dropped rather than stored.
    pub(crate) fn note_tick(&mut self, call_id: String, elapsed_secs: u64, lines: Option<u64>) {
        if !self.running.contains(&call_id) {
            return;
        }
        self.bash.insert(
            call_id,
            BashProgress {
                elapsed_secs,
                lines,
            },
        );
        self.revision = self.revision.wrapping_add(1);
    }

    /// The ticker entry for one call, or None when it has none.
    pub(crate) fn tick(&self, call_id: &str) -> Option<&BashProgress> {
        self.bash.get(call_id)
    }

    /// The revision of the ticker, for the slot cache identity.
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
}

/// One in-flight run: the request id the server correlates, the wall-clock
/// start, and the streaming progress. Started at the moment the enqueue
/// succeeded locally — the run is "running" from the App's perspective
/// even before the first frame arrives.
#[derive(Debug)]
pub struct ActiveRun {
    pub request: RequestId,
    pub started_at: Instant,
    pub(crate) progress: RunProgress,
}

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

    /// Borrow the streaming progress of the active run, or None when idle.
    pub fn progress(&self) -> Option<&RunProgress> {
        match self {
            RunState::Running(r) | RunState::Waiting(r) | RunState::Cancelling(r) => {
                Some(&r.progress)
            }
            RunState::Idle => None,
        }
    }

    /// Mutably borrow the streaming progress, or None when idle.
    pub fn progress_mut(&mut self) -> Option<&mut RunProgress> {
        match self {
            RunState::Running(r) | RunState::Waiting(r) | RunState::Cancelling(r) => {
                Some(&mut r.progress)
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
            progress: RunProgress::default(),
        });
    }

    /// Running → Waiting. No-op if not Running (a late permission ask after
    /// cancel or completion must not revive a dead run). Moves the whole
    /// ActiveRun so its progress carries across the pause unchanged.
    pub fn begin_waiting(&mut self) {
        let prev = std::mem::replace(self, RunState::Idle);
        *self = match prev {
            RunState::Running(run) => RunState::Waiting(run),
            other => other,
        };
    }

    /// Waiting → Running. No-op if not Waiting (a late verdict after the run
    /// ended must not flip a dead run back to Running). Moves the whole
    /// ActiveRun so its progress carries across the resume.
    pub fn end_waiting(&mut self) {
        let prev = std::mem::replace(self, RunState::Idle);
        *self = match prev {
            RunState::Waiting(run) => RunState::Running(run),
            other => other,
        };
    }

    /// Running/Waiting → Cancelling. No-op if Idle. Moves the whole ActiveRun
    /// so its progress carries across the cancel.
    pub fn begin_cancel(&mut self) {
        let prev = std::mem::replace(self, RunState::Idle);
        *self = match prev {
            RunState::Running(run) | RunState::Waiting(run) => RunState::Cancelling(run),
            other => other,
        };
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

#[cfg(test)]
mod tests;
