//! The background reads of the transcript's own log: send one off the draw
//! path, apply what comes back, and decide the fate of a row the whole-vec
//! viewport is parked on. The search view's read of the same log runs off the
//! draw path under the same contract and is pumped alongside, in the module
//! that owns the search view.
//!
//! The worker side runs one job and reports its outcome; this side owns when a
//! job is sent, which result still belongs to the view it lands in, and what
//! the reader is told when a place cannot be held. The record of a running read
//! and the budget bounding how many run at once live with the state that owns
//! them, so a change to the read's lifecycle lands here and building rows from
//! frames stays with the parent module.

use std::sync::mpsc;
use std::time::Duration;

use super::LOG_ROW_BUDGET;
use crate::app::state::App;
use crate::composition::shared_runtime;
use crate::session::run_control::history_read::{LookbackFrom, OVERLAP_FRONT_ROWS, ReadJob};
use crate::transcript::history_state::{
    HistoryReadOutcome, HistoryReadPoll, HistoryStamp, PendingHistoryRead,
};
use crate::transcript::row_id::RowId;
use crate::transcript::scroll::RowRelease;
use crate::transcript::store::DiskFront;
use crate::ui::notifications::{NotifKind, Notification};

impl App {
    /// Dispatch a background read of older rows from the session log, the
    /// rows of the drained frames that still live on disk. The draw path
    /// never blocks on this: the task ships the result over a channel the
    /// record on file holds, and a later pump applies it. Without a snapshot
    /// source or a connection there is nothing to dispatch and the scroll stops
    /// at the resident front; when the owner has no room for another read a
    /// second dispatch is skipped and a later scroll asks again, so the log is
    /// walked by one task at a time and a result nobody has looked at yet is
    /// never dispatched over.
    ///
    /// A reach names a row the read has to get back to, for a viewport parked
    /// on a row the rows on view no longer carry; no reach reads the older rows
    /// the scroll back reached. Returns whether a read was dispatched, so a
    /// caller that was reading for a parked row can act on the answer that none
    /// will come.
    pub(super) fn dispatch_history_read(&mut self, reach: Option<RowId>) -> bool {
        let Some(source) = self.sessions.persistence.snapshot.clone() else {
            return false;
        };
        if self.sessions.current.connection.is_none() {
            // No connection: a source with no connection is not a live
            // session's log, so there are no older rows to read, matching the
            // no-source short-circuit.
            return false;
        }
        if !self.sessions.current.history_reads.may_dispatch() {
            return false;
        }
        if self.sessions.current.transcript.disk_row_count() >= LOG_ROW_BUDGET {
            return false;
        }
        // The record carries the row this read was sent for, so a walk that
        // could not reach it decides that row's fate rather than whatever row
        // the reader stands on when the answer lands.
        let lookback = reach.clone();
        let job = match (self.sessions.current.transcript.disk_front(), reach) {
            // A closed front has nothing older to read, for a scroll back or
            // for a row the view no longer shows.
            (DiskFront::Stopped, _) => return false,
            (DiskFront::Unloaded, reach) => {
                // The pass that drained frames still renders their rows.
                // Re-derive the view first, so the rows the overlap matches
                // are the ones that stay: a match against a row on its way
                // out would place the loaded rows above history the next
                // rebuild drops.
                self.rebuild_transcript();
                let front = self.transcript().front_rows(OVERLAP_FRONT_ROWS);
                if front.is_empty() {
                    return false;
                }
                match reach {
                    Some(row) => ReadJob::Lookback {
                        source,
                        from: LookbackFrom::Visible(front),
                        row,
                    },
                    None => ReadJob::Tail { source, front },
                }
            }
            // The window ends where the loaded rows begin, so every row it
            // carries is older than every row the view holds and the overlap
            // needs no match.
            (DiskFront::At(anchor), Some(row)) => ReadJob::Lookback {
                source,
                from: LookbackFrom::Offset(anchor),
                row,
            },
            (DiskFront::At(anchor), None) => ReadJob::Before { source, anchor },
        };
        // Capture the resident front after any rebuild the Unloaded branch ran,
        // not before: that rebuild can drain frames and advance the front, so a
        // pre-rebuild capture would make the first read's own result look stale
        // and drop it. The sync path this replaced read the front after the
        // rebuild too.
        let dispatch_front = self.sessions.current.transcript.frame_window_start();
        let dispatch_disk_front = self.sessions.current.transcript.disk_front();
        let (tx, rx) = mpsc::channel();
        // The owner issues the stop flag and the execution slot together, so the
        // flag the task watches is the one an invalidation raises and the budget
        // counts the read from the moment the worker exists. The lease moves into
        // the closure and drops when the closure returns, which is what holds the
        // slot for the length of the walk: an invalidation before that point
        // stops the worker but cannot take the slot back from it.
        let history_reads = &mut self.sessions.current.history_reads;
        let stamp = HistoryStamp::new(dispatch_front, dispatch_disk_front, lookback);
        let lease = history_reads.dispatch(stamp, rx);
        shared_runtime().spawn_blocking(move || {
            job.run_and_send(tx, lease.stop());
        });
        true
    }

    /// Bring back the row the whole-vec viewport stands on when the rows on
    /// view no longer carry it. A drain or a release can take the row out from
    /// under a parked reader, and holding the place is only honest while the
    /// session log still has the row: this asks the log, and a row the log
    /// cannot give back releases the viewport with the reader told.
    ///
    /// The dispatch is direct rather than through the scroll-back load path,
    /// which reads only while the viewport stands near the resident front. A
    /// parked row has left the view and its screen row is wherever it last
    /// resolved, so that gate would hold off exactly the read this needs.
    fn pump_parked_row(&mut self) -> bool {
        if self.ui.teammate_view.is_some() || self.window_browsing() {
            return false;
        }
        if !self.whole_scroll().row_absent() {
            return false;
        }
        let Some(anchor) = self.whole_scroll().anchor().cloned() else {
            return false;
        };
        // A name no log entry carries never comes back from a read: a row the
        // frontend raised and a frame's own position each name a place in one
        // view. The viewport returns to the tail now rather than after a walk
        // that could not find the row.
        if !anchor.row.is_durable() {
            return self.release_parked_row(RowRelease::RowLeftHistory);
        }
        // A read is already running, and it may be the one this row waits for:
        // wait for its result instead of dispatching over it.
        if !self.sessions.current.history_reads.may_dispatch() {
            return false;
        }
        if self.dispatch_history_read(Some(anchor.row)) {
            return false;
        }
        // Nothing can read the log for this view: no source, no connection, no
        // room for more rows, or a front already closed. The row may well sit
        // in the log, so the reader is told the place is out of this view's
        // reach rather than that the row left the history: a closed front means
        // either a walk proved the log ends here or a worker died without
        // answering, and only the first would make the stronger claim true.
        self.release_parked_row(RowRelease::RowOutOfReach)
    }

    /// Advance the work that runs off the draw path: the older-rows read, the
    /// epoch scan, the search view's read of the log, and the read that brings
    /// back a parked row. All four read the log, so none runs where the frame
    /// is drawn; the loop calls this instead. Returns true when any moved, so
    /// the caller redraws.
    pub(crate) fn pump_background_reads(&mut self) -> bool {
        let history = self.pump_history_read();
        let scan = self.pump_scan_chunk();
        let search = self.pump_search_read();
        // After the read pump, so a result that landed in this pass has already
        // resolved the parked row: what is left for the chase is a row the rows
        // on view still leave out.
        let parked = self.pump_parked_row();
        history || scan || search || parked
    }

    /// Drain the running history read without blocking and apply it when
    /// ready. Returns true when a result landed and the view must redraw.
    /// Called from the loop body every pass, not inside the dirty gate, so an
    /// idle loop still picks the result up.
    pub(crate) fn pump_history_read(&mut self) -> bool {
        let Some(pending) = self.sessions.current.history_reads.take() else {
            return false;
        };
        match pending.poll() {
            HistoryReadPoll::Pending => {
                self.sessions.current.history_reads.put_back(pending);
                false
            }
            HistoryReadPoll::Ready(outcome) => {
                self.apply_history_read_result(outcome, &pending);
                true
            }
            HistoryReadPoll::Disconnected => {
                // The worker died or dropped its sender without sending. Leave
                // the record cleared and stop re-reading: without the latch the
                // scroll keeps asking for rows no worker will ever bring back.
                // The budget needs no help here: a worker frees its slot on its
                // own when it leaves.
                let transcript = &mut self.sessions.current.transcript;
                transcript.set_disk_front(DiskFront::Stopped);
                false
            }
        }
    }

    /// Apply a background read's result, or drop it when it no longer belongs
    /// to the view. Four checks, each covering one way the world moves while a
    /// read runs: the reader returned to the tail (rows not wanted); the
    /// history was invalidated (the epoch moved on, so the rows belong to an
    /// archived history even when the rebuilt view's numbers match again); the
    /// resident front moved (the frames the rows were cut against are gone and
    /// prepending would leave a gap); the disk-rows state moved (a chained read
    /// is trusted against the loaded stack's seam without its own overlap
    /// match, so releasing the stack removes the basis of that trust). Full
    /// view-generation rejection (pane, parent/child) is a later concern.
    fn apply_history_read_result(
        &mut self,
        outcome: HistoryReadOutcome,
        read: &PendingHistoryRead,
    ) {
        if self.whole_scroll().is_following_tail() {
            tracing::debug!("history read dropped: the reader returned to the tail");
            return;
        }
        if read.epoch != self.sessions.current.history_reads.epoch() {
            tracing::debug!(
                read_epoch = read.epoch,
                current = self.sessions.current.history_reads.epoch(),
                "history read dropped: the history was invalidated"
            );
            return;
        }
        if self.sessions.current.transcript.frame_window_start() != read.stamp.dispatch_front {
            tracing::debug!(
                dispatch_front = read.stamp.dispatch_front,
                current = self.sessions.current.transcript.frame_window_start(),
                "history read dropped: the resident front moved"
            );
            return;
        }
        if self.sessions.current.transcript.disk_front() != read.stamp.dispatch_disk_front {
            tracing::debug!(
                dispatch_disk_front = ?read.stamp.dispatch_disk_front,
                current = ?self.sessions.current.transcript.disk_front(),
                "history read dropped: the disk-rows state moved"
            );
            return;
        }
        match outcome {
            HistoryReadOutcome::Exhausted => {
                let transcript = &mut self.sessions.current.transcript;
                transcript.set_disk_front(DiskFront::Stopped);
                self.sessions.current.history_reads.record_read_success();
                // A read sent for a parked row that walked to the log's start
                // without it: no later read finds the row there either, so the
                // viewport gives up the place rather than holding one the
                // history cannot fill.
                self.release_chased_row(&read.stamp.lookback, RowRelease::RowLeftHistory);
            }
            HistoryReadOutcome::LookbackMissed => {
                // The walk spent its budget with the log still holding older
                // rows, so the front stays open: a row this view cannot reach
                // is not a log that ends here.
                tracing::debug!("lookback read stopped at its budget without the row");
                self.sessions.current.history_reads.record_read_success();
                self.release_chased_row(&read.stamp.lookback, RowRelease::RowOutOfReach);
            }
            HistoryReadOutcome::Rows(result) => {
                let transcript = &mut self.sessions.current.transcript;
                transcript.prepend_disk_rows(result.rows, result.anchor, read.stamp.dispatch_front);
                // The rows landed above the viewport; the rebuild resolves the
                // parked anchor over them, so the viewport holds its place by
                // the row it stands on rather than by counting what arrived.
                self.rebuild_transcript();
                self.bump_transcript_version();
                self.sessions.current.history_reads.record_read_success();
            }
            HistoryReadOutcome::ReadFailed => {
                // The front stays open for a retry after the cooldown; the
                // failure surfaces as a visible notice.
                tracing::warn!("history read failed; front stays open for a retry");
                self.sessions.current.history_reads.record_read_failure();
                self.ui.notifications.add(Notification::immediate(
                    "history-read-failed",
                    NotifKind::text("older rows unreadable — retrying shortly"),
                    Duration::from_millis(4000),
                ));
            }
        }
    }

    /// Give up a parked row when the read sent for it came back without it.
    ///
    /// The read decides the fate of the row it was sent for, and of no other:
    /// the reader may have parked elsewhere while it ran, and a row that came
    /// back by some other route is not absent any more, so both are checked
    /// here rather than at the dispatch that sent the read.
    fn release_chased_row(&mut self, chased: &Option<RowId>, verdict: RowRelease) {
        let Some(chased) = chased.as_ref() else {
            return;
        };
        if self.whole_scroll().anchor().map(|anchor| &anchor.row) != Some(chased) {
            return;
        }
        if !self.whole_scroll().row_absent() {
            return;
        }
        self.release_parked_row(verdict);
    }
}
