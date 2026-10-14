//! The background history reads of the working transcript: the read that is
//! running, and the history generation it was dispatched against.
//!
//! The transcript facts decide whether a landed result still fits the view;
//! this owner decides whether the read is still wanted. An invalidation bumps
//! the epoch and drops the record, and the drop raises the worker's stop
//! signal, so a late result has nowhere to land and the worker producing it
//! quits at its next window instead of walking the rest of its byte budget for
//! rows nothing will ever apply.
//!
//! The budget counts workers, not records. Dropping a record tells a worker to
//! stop, but one already inside a read cannot observe that until it comes back
//! up for its next window, so the slot it holds stays taken until it does. A
//! count taken from the record would instead report the budget free while two
//! walks of the same log run at once.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

use crate::records::TranscriptLine;
use crate::state::transcript::DiskFront;
use mpsc::TryRecvError;

/// History reads running at once.
///
/// A second walk of the same log re-reads bytes the first is already bringing
/// back, and its result is thrown away anyway: the first landing moves the disk
/// rows on, and the apply side rejects a read stamped with the state before
/// that move. So exceeding the budget spends IO for nothing, and a dispatch
/// waits for the running read to clear. The budget is stated here and asserted
/// at the dispatch point, against a count of the workers holding a slot rather
/// than of the records the owner keeps.
pub(crate) const HISTORY_READ_WORKER_BUDGET: usize = 1;

/// The handle a worker watches to learn it is no longer wanted.
///
/// A read already blocking inside the kernel cannot be cancelled, so this bounds
/// what a worker does after its result has been dropped: one more window, not
/// the rest of its walk. It says nothing about kernel IO, which finishes or
/// fails on its own schedule.
pub(crate) type StopSignal = Arc<AtomicBool>;

/// An execution slot, held for as long as the worker holding it runs.
///
/// Freed by the worker leaving rather than by the owner letting go of its
/// record, because the budget counts what is running.
///
/// Releasing on drop also covers a task that never runs: a runtime shutting down
/// drops the closure it was handed, which drops this with it, so a slot cannot
/// leak and refuse every later read.
pub(crate) struct WorkerPermit {
    active: Arc<AtomicUsize>,
}

impl WorkerPermit {
    /// Take a slot, counted from the moment the worker exists.
    fn acquire(active: &Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::AcqRel);
        Self {
            active: Arc::clone(active),
        }
    }
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What a dispatch hands the task it starts, and what that task holds for its
/// whole run: the flag telling it whether the read is still wanted, and the slot
/// the budget counts.
///
/// Both are issued together here, in the owner whose record drop raises the flag
/// and whose counter the slot belongs to, and neither gives the worker a route
/// to that record. Holding them in one value ties the slot's lifetime to the
/// worker's rather than to the caller's: the lease moves into the task, so the
/// slot comes back when the task does and not before.
#[must_use = "dropping a lease frees its slot and discards its stop flag"]
pub(crate) struct WorkerLease {
    stop: StopSignal,
    _permit: WorkerPermit,
}

impl WorkerLease {
    /// The flag the worker checks between windows.
    pub(crate) fn stop(&self) -> &StopSignal {
        &self.stop
    }
}

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

/// A dispatched history read, stamped with everything the apply side needs to
/// decide whether the result still belongs to the view: the history
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
    /// clears the record, a session switch replaces the whole owner, and a quit
    /// drops it with the rest of the app. Raising the flag here rather than at
    /// each of those keeps the abandon paths from having to agree. A record
    /// dropped after its result landed raises a flag nobody reads again.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// What poll sees: still waiting, a result ready to apply, or the background
/// task gone. Disconnected is distinct from Pending so a worker that died
/// without sending is still cleared from the record instead of being waited on
/// forever, which is also what latches the front against a re-dispatch storm.
#[derive(Debug)]
pub(crate) enum HistoryReadPoll {
    Pending,
    Ready(HistoryReadOutcome),
    Disconnected,
}

impl PendingHistoryRead {
    /// Drain the channel without blocking. Pending keeps the record; Ready
    /// returns the outcome for the caller to apply and clear it; Disconnected
    /// means the worker is gone and the record must be cleared.
    pub fn poll(&self) -> HistoryReadPoll {
        match self.rx.try_recv() {
            Ok(outcome) => HistoryReadPoll::Ready(outcome),
            Err(TryRecvError::Empty) => HistoryReadPoll::Pending,
            Err(TryRecvError::Disconnected) => HistoryReadPoll::Disconnected,
        }
    }
}

/// The reads of one transcript history: the read on file, the workers holding a
/// slot against the budget, and the generation counter that outlives both.
/// Lives on App beside the scroll state the apply side consults, not inside
/// Transcript: the transcript owns the facts, while this owner governs a task
/// lifecycle across those facts being reset.
///
/// The worker count belongs here rather than to the task runner because it is
/// the owner that decides a read is unwanted, and a read it has just stopped is
/// still running. A session switch rebuilds this whole owner, so the count the
/// new session dispatches against starts at zero and inherits nothing from the
/// worker the old session abandoned.
#[derive(Debug, Default)]
pub struct HistoryReads {
    /// Bumped on every history invalidation. Strictly monotonic within one
    /// App; a session switch rebuilds the whole owner instead.
    epoch: u64,
    pending: Option<PendingHistoryRead>,
    /// Slots the dispatched workers hold. Shared with every permit handed out,
    /// so the budget reads what is executing rather than what the record above
    /// still names.
    active: Arc<AtomicUsize>,
}

impl HistoryReads {
    /// The current history generation. A landed read compares its stamp to
    /// this before applying.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// How many reads are running: the workers holding a slot, not the records
    /// this owner keeps. A stopped worker holds its slot until its read ends.
    pub(crate) fn running(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// Whether a dispatched read is still on file, so a result may yet land and
    /// a poll has something to look at.
    ///
    /// Not the same question as running(), and the two come apart in both
    /// directions: a worker can have left with its result sitting in the
    /// channel, and an invalidation can clear the record while its worker still
    /// walks.
    pub(crate) fn holds_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether there is room to dispatch another read.
    ///
    /// Two things say no, and neither implies the other. A worker still holding
    /// a slot is walking the log, so a second dispatch walks the same bytes
    /// again for one answer. A record still on file is a result nobody has
    /// looked at yet, and dispatching over it throws that result away -- its
    /// worker may already have left and freed the budget, which is why the
    /// count alone cannot answer this.
    pub(crate) fn may_dispatch(&self) -> bool {
        self.running() < HISTORY_READ_WORKER_BUDGET && !self.holds_pending()
    }

    /// Record a dispatched read, stamped with the current epoch and the
    /// transcript coordinates the apply side re-checks.
    ///
    /// Returns the lease the caller moves into the task it starts, so the flag
    /// that task watches is created here, in the owner whose drop raises it, and
    /// the slot the budget counts is taken at the same moment. Neither gives the
    /// task a route to a record it should not touch. Dispatching with no room
    /// for it is a programming error rather than a runtime condition: the guard
    /// is may_dispatch and it sits at the dispatch point, so reaching this
    /// method means a caller skipped it.
    #[must_use = "the lease is what the task holds, and it frees the slot on drop"]
    pub(crate) fn dispatch(
        &mut self,
        dispatch_front: usize,
        dispatch_disk_front: DiskFront,
        rx: mpsc::Receiver<HistoryReadOutcome>,
    ) -> WorkerLease {
        debug_assert!(
            self.may_dispatch(),
            "a history read was dispatched with no room for it: {} running \
             against a budget of {}, and a record on file = {}",
            self.running(),
            HISTORY_READ_WORKER_BUDGET,
            self.holds_pending()
        );
        let permit = WorkerPermit::acquire(&self.active);
        let stop = Arc::new(AtomicBool::new(false));
        self.pending = Some(PendingHistoryRead {
            epoch: self.epoch,
            dispatch_front,
            dispatch_disk_front,
            stop: Arc::clone(&stop),
            rx,
        });
        WorkerLease {
            stop,
            _permit: permit,
        }
    }

    /// Take the read on file out for polling. On Ready or Disconnected the
    /// caller leaves it cleared by not putting it back; on Pending it must put
    /// the record back to keep the read alive.
    pub(crate) fn take(&mut self) -> Option<PendingHistoryRead> {
        self.pending.take()
    }

    /// Put a pending read back after a Pending poll. Single-caller contract:
    /// only the loop's pump holds a taken record, and it puts back before
    /// anything else can dispatch, so nothing is on file on return.
    pub(crate) fn put_back(&mut self, read: PendingHistoryRead) {
        debug_assert!(
            self.pending.is_none(),
            "put_back must not clobber a read already on file"
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
    /// It does not free the budget. The slot belongs to the worker and comes
    /// back when the worker does, so a stop that has not yet been observed still
    /// counts against the limit it exists to keep.
    ///
    /// The generation counter covers what a flag cannot reach: a record this
    /// owner no longer holds is still refused by its stamp when it lands.
    pub(crate) fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.pending = None;
    }
}

#[cfg(test)]
mod tests;
