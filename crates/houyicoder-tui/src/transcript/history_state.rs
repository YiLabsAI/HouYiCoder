//! The working transcript's older-row read: what a landed result says, and the
//! coordinates its apply side re-checks before applying it.
//!
//! The read's lifecycle -- the record on file, the worker budget, the stop
//! signal, and the generation -- is the shared one in the sibling lifecycle
//! module, because a second read of the same log needs the same bounds and two
//! copies of them would have to be kept in agreement. What is specific here is
//! the stamp: the transcript facts that decide whether rows which landed still
//! belong above the view that asked for them.

use crate::state::transcript::DiskFront;
use crate::transcript::read_lifecycle::{PendingRead, ReadLifecycle, ReadPoll};
use crate::transcript::row_id::RowId;
use crate::transcript::rows::NamedRows;

/// Rows a background read brought back from the session log, each with its
/// identity, plus the byte offset the next older read ends at.
#[derive(Debug)]
pub(crate) struct HistoryReadResult {
    pub rows: NamedRows,
    pub anchor: u64,
}

/// What a background read returned: rows to prepend, that the log holds
/// nothing older to show, or that the read failed and the front stays open for
/// a retry.
#[derive(Debug)]
pub(crate) enum HistoryReadOutcome {
    Rows(HistoryReadResult),
    Exhausted,
    /// A read sent for a row the view no longer shows walked its whole budget
    /// without reaching it. Unlike Exhausted the log is not proven to hold no
    /// older row -- the walk stopped at its own limit, not at the log's start.
    LookbackMissed,
    /// An IO failure reading the log. Unlike Exhausted, the log may yet hold
    /// older rows: the front stays open and a retry follows the cooldown.
    ReadFailed,
}

/// The transcript coordinates a landed read is re-checked against, in addition
/// to the generation it was stamped with.
#[derive(Debug)]
pub(crate) struct HistoryStamp {
    /// The resident front at dispatch. A drain that moves it before the result
    /// lands makes the result stale.
    pub dispatch_front: usize,
    /// The disk-rows state at dispatch. A chained read is cut against the
    /// loaded stack seam without its own overlap match; when the stack is
    /// released the basis of that trust is gone and the result must not apply.
    pub dispatch_disk_front: DiskFront,
    /// The row this read was sent to reach, when the viewport was parked on a
    /// row the rows on view left out. A read that could not reach it decides
    /// the fate of that row alone: the reader may park elsewhere while the read
    /// runs, and a verdict about one row says nothing about another.
    pub lookback: Option<RowId>,
}

impl HistoryStamp {
    /// Stamp a dispatch with the coordinates the apply side compares against.
    pub(crate) fn new(
        dispatch_front: usize,
        dispatch_disk_front: DiskFront,
        lookback: Option<RowId>,
    ) -> Self {
        Self {
            dispatch_front,
            dispatch_disk_front,
            lookback,
        }
    }
}

/// The read on file for this transcript history.
pub(crate) type PendingHistoryRead = PendingRead<HistoryStamp, HistoryReadOutcome>;

/// What a poll of that read sees.
pub(crate) type HistoryReadPoll = ReadPoll<HistoryReadOutcome>;

/// The older-row reads of one transcript history.
pub(crate) type HistoryReads = ReadLifecycle<HistoryStamp, HistoryReadOutcome>;

#[cfg(test)]
mod tests;
