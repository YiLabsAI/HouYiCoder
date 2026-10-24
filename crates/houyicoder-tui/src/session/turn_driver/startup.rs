//! The resume handshake: enqueue the status query, then drain the frames a
//! resumed session ships ahead of the reply, applying them in batches.

use std::time::{Duration, Instant};

use crate::agent_message::ClientCommand;
use crate::agent_message::{ServerEvent, SessionMessage};
use crate::state::App;
use crate::transcript::SequencedFrame;

/// The estimated frame bytes a startup batch may hold before it is applied.
/// A resumed session ships its history as a run of frames ahead of the status
/// reply, and the drain stops holding that run once it reaches this bound, so
/// a history longer than the bound is applied as it arrives.
pub(super) const STARTUP_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// The run of frames a startup drain holds before applying them.
///
/// The frames and the byte count they are measured by belong to one value: a
/// drain empties both, so the next run starts from zero. Holding a frame that
/// reaches the bound hands the run back to be applied, and the paths that
/// reach another message take the run as it stands.
pub(super) struct StartupBatch {
    bound: usize,
    bytes: usize,
    frames: Vec<SequencedFrame>,
}

impl StartupBatch {
    pub(super) fn new(bound: usize) -> Self {
        Self {
            bound,
            bytes: 0,
            frames: Vec::new(),
        }
    }

    /// Hold a frame, returning the run to apply once the bound is reached.
    pub(super) fn push(&mut self, frame: SequencedFrame) -> Option<Vec<SequencedFrame>> {
        self.bytes += frame.estimated_bytes();
        self.frames.push(frame);
        (self.bytes >= self.bound).then(|| self.take())
    }

    /// Take the held run, leaving the batch empty.
    pub(super) fn take(&mut self) -> Vec<SequencedFrame> {
        self.bytes = 0;
        self.frames.drain(..).collect()
    }
}

impl App {
    /// Enqueue the status query, then drain startup messages by type until the
    /// trust gate resolves. Ready arrives before any trust prompt, so the drain
    /// keeps reading past it: the loop dispatches each polled message by type
    /// and stops once the trust card is raised or the status reply proves the
    /// workspace passed the gate.
    pub fn startup_handshake(&mut self, timeout: Duration) {
        self.drain_startup(timeout, STARTUP_BATCH_BYTES);
    }

    /// The startup drain, with the batch bound its caller chooses. The bound a
    /// run of frames is applied at is what the drain's own tests drive, so they
    /// can take a history past it without queueing megabytes of frames.
    pub(super) fn drain_startup(&mut self, timeout: Duration, batch_bound: usize) {
        if let Some(session) = self.sessions.current.connection.as_ref() {
            match session.next_request_id() {
                Ok(req_id) => {
                    if session
                        .enqueue(ClientCommand::StatusQuery { req_id })
                        .is_err()
                    {
                        // Closed: the ConnectionLost event announces the loss.
                    }
                }
                Err(_) => self.note_request_id_exhausted(),
            }
        }
        let deadline = Instant::now() + timeout;
        // A resumed session ships its whole history as a run of frames ahead of
        // the status reply. Applying them one at a time would rebuild the
        // transcript once per frame, so the cost grows with the square of the
        // history; applying the run whole would hold a history of any length
        // before any of it reaches the screen. The run lands in batches as it
        // arrives instead, and the frames before another message are applied
        // before it, so the transcript is current when that message is handled.
        let mut batch = StartupBatch::new(batch_bound);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let connection = self.sessions.current.connection.as_mut();
            let next = connection.and_then(|s| s.poll_startup(remaining));
            let Some(message) = next else {
                break;
            };
            match message {
                SessionMessage::Event(ServerEvent::Frame(frame)) => {
                    if let Some(ready) = batch.push(frame) {
                        self.apply_frames(ready);
                    }
                }
                other => {
                    self.apply_frames(batch.take());
                    self.handle_agent_message(other);
                }
            }
            let status = &self.sessions.current.capabilities.status;
            let status_present = status.snapshot().is_some();
            if self.pending_trust().is_some() || status_present {
                break;
            }
        }
        self.apply_frames(batch.take());
    }
}
