//! The background history reads of the working transcript: the read that is
//! running, and the history generation it was dispatched against.
//!
//! The transcript facts decide whether a landed result still fits the view;
//! this owner decides whether the read is still wanted. An invalidation bumps
//! the epoch and drops the record, and the drop raises the worker's stop
//! signal, so a late result has nowhere to land and the worker producing it
//! quits at its next window instead of walking the rest of its byte budget for
//! rows nothing will ever apply.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use crate::records::TranscriptLine;
use crate::state::transcript::DiskFront;

/// History reads running at once.
///
/// A second walk of the same log re-reads bytes the first is already bringing
/// back, and its result is thrown away anyway: the first landing moves the disk
/// rows on, and the apply side rejects a read stamped with the state before
/// that move. So exceeding the budget spends IO for nothing, and a dispatch
/// waits for the running read to clear. The budget is stated here and asserted
/// at the dispatch point rather than left to the shape of the slot holding the
/// read.
pub(crate) const HISTORY_READ_WORKER_BUDGET: usize = 1;

/// The handle a worker watches to learn it is no longer wanted.
///
/// A read already blocking inside the kernel cannot be cancelled, so this bounds
/// what a worker does after its result has been dropped: one more window, not
/// the rest of its walk. It says nothing about kernel IO, which finishes or
/// fails on its own schedule.
pub(crate) type StopSignal = Arc<AtomicBool>;

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
/// sender and drops it on exit. The stop flag is shared with that task, and
/// dropping this record raises it, so every way a read stops being wanted
/// tells the worker rather than only silencing it.
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
    /// Raised when the read stops being wanted. The owner holds it here and the
    /// task holds its own reference to the same flag, so the one that decides
    /// the read is unwanted is the one that says so.
    stop: StopSignal,
    rx: mpsc::Receiver<HistoryReadOutcome>,
}

impl std::fmt::Debug for PendingHistoryRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingHistoryRead")
            .field("epoch", &self.epoch)
            .field("dispatch_front", &self.dispatch_front)
            .field("dispatch_disk_front", &self.dispatch_disk_front)
            .field("stop", &self.stop.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Drop for PendingHistoryRead {
    /// Tell the worker its rows are unwanted on the way out.
    ///
    /// Every way a read stops being wanted ends in this drop: an invalidation
    /// clears the slot, a session switch replaces the whole owner, and a quit
    /// drops it with the rest of the app. Raising the flag here rather than at
    /// each of those keeps the abandon paths from having to agree. A record
    /// dropped after its result landed raises a flag nobody reads again.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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

    /// How many reads are running. A dispatch is skipped while this is at the
    /// budget and a later scroll asks again, so one task walks the log at a
    /// time. Callers ask the count rather than the slot holding the read, so
    /// the limit they are keeping lives beside the constant that names it.
    pub(crate) fn running(&self) -> usize {
        usize::from(self.pending.is_some())
    }

    /// Record a dispatched read, stamped with the current epoch and the
    /// transcript coordinates the apply side re-checks.
    ///
    /// Returns the stop flag the caller hands the task it starts, so the flag
    /// is created here, in the owner whose drop raises it, and the task never
    /// has a route to a record it should not touch. Dispatching past the budget is a
    /// programming error rather than a runtime condition: the guard sits at the
    /// dispatch point, and reaching this method means a caller skipped it.
    #[must_use = "the flag returned here is the only way to stop the read"]
    pub(crate) fn dispatch(
        &mut self,
        dispatch_front: usize,
        dispatch_disk_front: DiskFront,
        rx: mpsc::Receiver<HistoryReadOutcome>,
    ) -> StopSignal {
        debug_assert!(
            self.running() < HISTORY_READ_WORKER_BUDGET,
            "a history read was dispatched while {} were running, past the budget of {}",
            self.running(),
            HISTORY_READ_WORKER_BUDGET
        );
        let stop = Arc::new(AtomicBool::new(false));
        self.pending = Some(PendingHistoryRead {
            epoch: self.epoch,
            dispatch_front,
            dispatch_disk_front,
            stop: Arc::clone(&stop),
            rx,
        });
        stop
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
    /// generation.
    ///
    /// Dropping the receiver ends the result route here, but on its own it
    /// leaves the worker walking: it finds out only at the send, which for a
    /// tail read comes after the rest of its walk back through the log, for
    /// rows nothing will ever apply. The drop raises the read's stop flag too,
    /// so the worker quits at its next window instead.
    ///
    /// The generation counter covers what a flag cannot reach: a record this
    /// owner no longer holds is still refused by its stamp when it lands.
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
        drop(reads.dispatch(0, DiskFront::Unloaded, rx));
        assert_eq!(reads.running(), 1);
        let epoch_before = reads.epoch();

        reads.invalidate();

        assert_eq!(reads.running(), 0, "the running read was dropped");
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
        drop(reads.dispatch(3, DiskFront::At(4096), rx));
        let pending = reads.take().expect("a read is pending");
        assert_eq!(pending.epoch, reads.epoch());
        assert_eq!(pending.dispatch_front, 3);
        assert_eq!(pending.dispatch_disk_front, DiskFront::At(4096));
    }

    /// An invalidation raises the flag the dispatched worker holds, which is the
    /// half of the stop contract the owner owns: it cannot cancel a read already
    /// in the kernel, so it tells the worker its rows are unwanted and the
    /// worker quits at its next window. The next dispatch gets a fresh flag,
    /// because a raised one would stop the read it was just handed to.
    #[test]
    fn test_invalidate_raises_stop() {
        let mut reads = HistoryReads::default();
        let (_tx, rx) = mpsc::channel::<HistoryReadOutcome>();
        let stop = reads.dispatch(0, DiskFront::Unloaded, rx);
        assert!(!stop.load(Ordering::Relaxed), "a fresh read is wanted");

        reads.invalidate();

        assert!(
            stop.load(Ordering::Relaxed),
            "the dropped read was told to stop, not merely unheard"
        );
        let (_tx, rx) = mpsc::channel::<HistoryReadOutcome>();
        let next = reads.dispatch(0, DiskFront::Unloaded, rx);
        assert!(
            !next.load(Ordering::Relaxed),
            "a new read does not inherit the stop it was not given"
        );
        assert_ne!(
            Arc::as_ptr(&stop),
            Arc::as_ptr(&next),
            "each dispatch hands out its own flag"
        );
    }
}
