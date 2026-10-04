//! The background history reads of the working transcript: the read that is
//! running, and the history generation it was dispatched against.
//!
//! The transcript facts decide whether a landed result still fits the view;
//! this owner decides whether the read is still wanted. An invalidation bumps
//! the epoch and drops the record, so a late result has nowhere to land. The
//! worker is not stopped: its send fails into the dropped receiver.

use std::sync::mpsc;

use crate::records::TranscriptLine;
use crate::state::transcript::DiskFront;

/// Rows a background read brought back from the session log, with the byte
/// offset the next older read ends at.
#[derive(Debug)]
pub(crate) struct HistoryReadResult {
    pub rows: Vec<TranscriptLine>,
    pub anchor: u64,
}

/// What a background read returned: rows to prepend, or that the log holds
/// nothing older to show.
#[derive(Debug)]
pub(crate) enum HistoryReadOutcome {
    Rows(HistoryReadResult),
    Exhausted,
}

/// A history read while its task runs, stamped with everything the apply side
/// needs to decide whether the result still belongs to the view: the history
/// generation, the resident front, and the disk-rows state at dispatch. The
/// receiver is the channel the task sends the outcome over; the task owns the
/// sender and drops it on exit.
pub(crate) struct PendingHistoryRead {
    /// The owner epoch when the read was dispatched. A reset moves the epoch
    /// on, and a result from an older generation never applies even when the
    /// rebuilt view numbers match the dispatch again.
    pub epoch: u64,
    /// The resident front at dispatch. A drain that moves it before the result
    /// lands makes the result stale.
    pub dispatch_front: usize,
    /// The disk-rows state at dispatch. A chained read is cut against the
    /// loaded stack seam without its own overlap match; when the stack is
    /// released the basis of that trust is gone and the result must not apply.
    pub dispatch_disk_front: DiskFront,
    rx: mpsc::Receiver<HistoryReadOutcome>,
}

impl std::fmt::Debug for PendingHistoryRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingHistoryRead")
            .field("epoch", &self.epoch)
            .field("dispatch_front", &self.dispatch_front)
            .field("dispatch_disk_front", &self.dispatch_disk_front)
            .finish_non_exhaustive()
    }
}

/// What poll sees: still waiting, a result ready to apply, or the background
/// task gone. Disconnected is distinct from Pending so a dead worker cannot
/// pin the slot forever and block every later read.
#[derive(Debug)]
pub(crate) enum HistoryReadPoll {
    Pending,
    Ready(HistoryReadOutcome),
    Disconnected,
}

impl PendingHistoryRead {
    /// Drain the channel without blocking. Pending keeps the slot; Ready
    /// returns the outcome for the caller to apply and clear it; Disconnected
    /// means the worker is gone and the slot must be cleared.
    pub fn poll(&self) -> HistoryReadPoll {
        use mpsc::TryRecvError;
        match self.rx.try_recv() {
            Ok(outcome) => HistoryReadPoll::Ready(outcome),
            Err(TryRecvError::Empty) => HistoryReadPoll::Pending,
            Err(TryRecvError::Disconnected) => HistoryReadPoll::Disconnected,
        }
    }
}

/// The reads of one transcript history: at most one running read, and the
/// generation counter that outlives it. Lives on App beside the scroll state
/// the apply side consults, not inside Transcript: the transcript owns the
/// facts, while this owner governs a task lifecycle across those facts being
/// reset.
#[derive(Debug, Default)]
pub struct HistoryReads {
    /// Bumped on every history invalidation. Strictly monotonic within one
    /// App; a session switch rebuilds the whole owner instead.
    epoch: u64,
    pending: Option<PendingHistoryRead>,
}

impl HistoryReads {
    /// The current history generation. A landed read compares its stamp to
    /// this before applying.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Whether a read is running. A dispatch is skipped while one is pending,
    /// so at most one worker reads the log at a time.
    pub(crate) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Record a dispatched read, stamped with the current epoch and the
    /// transcript coordinates the apply side re-checks.
    pub(crate) fn dispatch(
        &mut self,
        dispatch_front: usize,
        dispatch_disk_front: DiskFront,
        rx: mpsc::Receiver<HistoryReadOutcome>,
    ) {
        self.pending = Some(PendingHistoryRead {
            epoch: self.epoch,
            dispatch_front,
            dispatch_disk_front,
            rx,
        });
    }

    /// Take the running read out for polling. On Ready or Disconnected the
    /// caller leaves the slot empty by not putting it back; on Pending it must
    /// put the record back to keep the read alive.
    pub(crate) fn take(&mut self) -> Option<PendingHistoryRead> {
        self.pending.take()
    }

    /// Put a pending read back after a Pending poll. Single-caller contract:
    /// only the loop's pump holds a taken record, and it puts back before
    /// anything else can dispatch, so the slot is empty on return.
    pub(crate) fn put_back(&mut self, read: PendingHistoryRead) {
        debug_assert!(
            self.pending.is_none(),
            "put_back must not clobber an occupied slot"
        );
        self.pending = Some(read);
    }

    /// A history invalidation: drop the running read and move to a new
    /// generation. Dropping the receiver ends the result route here; the epoch
    /// move guards the case where a record was already taken out and a
    /// rebuilt view matches the old dispatch numbers again.
    pub(crate) fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invalidate drops the running read and moves the generation on, so
    /// a held record from before is told apart by its stamp alone.
    #[test]
    fn test_invalidate_moves_epoch() {
        let mut reads = HistoryReads::default();
        let (tx, rx) = mpsc::channel::<HistoryReadOutcome>();
        reads.dispatch(0, DiskFront::Unloaded, rx);
        assert!(reads.is_pending());
        let epoch_before = reads.epoch();

        reads.invalidate();

        assert!(!reads.is_pending(), "the running read was dropped");
        assert_ne!(reads.epoch(), epoch_before, "the generation moved on");
        // The dropped receiver makes the worker send fail; nothing panics.
        assert!(tx.send(HistoryReadOutcome::Exhausted).is_err());
    }

    /// A dispatch after an invalidation carries the new generation and both
    /// transcript coordinates, so the apply side compares against the history
    /// the read actually saw.
    #[test]
    fn test_dispatch_stamps_epoch() {
        let mut reads = HistoryReads::default();
        reads.invalidate();
        let (_tx, rx) = mpsc::channel::<HistoryReadOutcome>();
        reads.dispatch(3, DiskFront::At(4096), rx);
        let pending = reads.take().expect("a read is pending");
        assert_eq!(pending.epoch, reads.epoch());
        assert_eq!(pending.dispatch_front, 3);
        assert_eq!(pending.dispatch_disk_front, DiskFront::At(4096));
    }
}
