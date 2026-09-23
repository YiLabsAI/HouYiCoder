//! The running history read: a background task reads older rows from the
//! session log while the draw path stays off disk, and ships the result over
//! a channel this slot holds. The types live apart from the Transcript struct
//! so the struct file stays under its size floor; Transcript owns the slot.

use crate::records::TranscriptLine;

/// Rows a background read brought back from the session log, with the byte
/// offset the next older read ends at.
#[derive(Debug)]
pub(crate) struct HistoryReadResult {
    pub rows: Vec<TranscriptLine>,
    pub anchor: u64,
}

/// What a background read returned: rows to prepend, or that the log holds
/// nothing older to show (the read reached the log start or found no match).
#[derive(Debug)]
pub(crate) enum HistoryReadOutcome {
    Rows(HistoryReadResult),
    Exhausted,
}

/// A history read while its task runs. The dispatch captures the resident front at
/// dispatch time so a drain that moves it before the result lands makes the
/// result stale. The receiver is the channel the background task sends the
/// outcome over; the task owns the sender and drops it on exit.
pub(crate) struct PendingHistoryRead {
    pub dispatch_front: usize,
    rx: std::sync::mpsc::Receiver<HistoryReadOutcome>,
}

impl std::fmt::Debug for PendingHistoryRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingHistoryRead")
            .field("dispatch_front", &self.dispatch_front)
            .finish_non_exhaustive()
    }
}

/// What poll sees: still waiting, a result ready to apply, or the background
/// task gone (panicked or dropped its sender). Disconnected is distinct from
/// Pending so a dead worker cannot pin the slot forever and block every later
/// read.
#[derive(Debug)]
pub(crate) enum HistoryReadPoll {
    Pending,
    Ready(HistoryReadOutcome),
    Disconnected,
}

impl PendingHistoryRead {
    /// Build a pending slot from the dispatch front and the channel the
    /// background task sends over.
    pub(crate) fn new(
        dispatch_front: usize,
        rx: std::sync::mpsc::Receiver<HistoryReadOutcome>,
    ) -> Self {
        Self { dispatch_front, rx }
    }

    /// Drain the channel without blocking. Pending keeps the slot; Ready
    /// returns the outcome for the caller to apply and clear it; Disconnected
    /// means the worker is gone and the slot must be cleared.
    pub fn poll(&self) -> HistoryReadPoll {
        use std::sync::mpsc::TryRecvError;
        match self.rx.try_recv() {
            Ok(outcome) => HistoryReadPoll::Ready(outcome),
            Err(TryRecvError::Empty) => HistoryReadPoll::Pending,
            Err(TryRecvError::Disconnected) => HistoryReadPoll::Disconnected,
        }
    }
}
