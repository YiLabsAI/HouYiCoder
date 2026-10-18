//! The background read that brings older rows back from the session log.
//!
//! The ReadJob is what the draw path hands to a background task: it carries the
//! source and the coordinates of the rows already shown, runs the walk, and
//! sends back what the log holds older than them. The walk holds one window at
//! a time and checks its stop flag between windows, so a read nobody is waiting
//! for quits after the window it is inside rather than after the rest of its
//! walk.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::scroll::WINDOW_MAX_BYTES;
use crate::state::history_read::{HistoryReadOutcome, HistoryReadResult};
use crate::transcript::row_id::RowId;
use crate::transcript::rows::NamedRows;
use crate::transcript::snapshot::{ReadError, TranscriptSnapshot, WindowLoad};
use crate::transcript::store::FrontRow;
use std::sync::mpsc::Sender;

/// Byte budget of one older-row read from the session log, and of each window
/// the first read walks back through.
const LOG_WINDOW_BYTES: u64 = WINDOW_MAX_BYTES;
/// How much log the first read walks back through before it gives up. The row
/// the visible transcript starts at sits behind the resident window, and the
/// log bytes that row sits behind are at least the bytes the resident rows
/// took to render; the walk holds one window at a time, so this bounds how far
/// it travels, not what it holds. Hitting the cap stops the read.
const LOG_WALK_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// How far past the rows a view holds a read for a parked row walks. Counted
/// from where those rows begin, not from the log tail: the read that reaches
/// them is the one an ordinary scroll back spends, and this bounds only what
/// the view reads past the rows it already shows. A walk that spends it ends
/// without the row, which the view reports rather than holding a place no read
/// reaches.
const LOOKBACK_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// How many rows from the visible front an overlap match is looked for in.
pub(crate) const OVERLAP_FRONT_ROWS: usize = 32;
/// How many rows an overlap match must hold for, so a row text the log repeats
/// (a one-word prompt) cannot cut the window at the wrong place.
const OVERLAP_RUN_ROWS: usize = 3;

/// A read the draw path handed to a background task. The task runs it to
/// completion and sends the outcome over the channel the record on file holds.
pub(crate) enum ReadJob {
    Tail {
        source: Arc<dyn TranscriptSnapshot>,
        front: Vec<FrontRow>,
    },
    Before {
        source: Arc<dyn TranscriptSnapshot>,
        anchor: u64,
    },
    /// Read older windows until the window holding this row is reached. Sent
    /// when the viewport parks on a row the rows on view left out: the row is
    /// the place to hold, and whether the log still has it decides between
    /// showing it again and telling the reader the place is gone.
    Lookback {
        source: Arc<dyn TranscriptSnapshot>,
        from: LookbackFrom,
        row: RowId,
    },
}

/// Where a lookback walk starts. The two forms are the two states a view can
/// be in: the rows it holds came from the log and it knows where they begin,
/// or none have been read back yet and the walk first has to reach them.
pub(crate) enum LookbackFrom {
    /// The rows the view shows, oldest first, for a view that has read nothing
    /// back yet. The walk reaches the rows older than them the way the first
    /// scroll back does.
    Visible(Vec<FrontRow>),
    /// The byte offset the rows already read back begin at.
    Offset(u64),
}

impl ReadJob {
    /// Run the job and report its outcome, unless the read was stopped while it
    /// ran.
    ///
    /// A stopped read reports nothing, because neither outcome would be true of
    /// the view that stopped it: no older row would latch the disk front closed
    /// on a view that has older rows to show, and rows would be handed back cut
    /// against a resident front that is already gone. Nothing is lost by the
    /// silence, since the flag is raised by the invalidation that dropped the
    /// record holding the receiver, so there is no apply side left to tell.
    fn run(self, stop: &AtomicBool) -> Option<HistoryReadOutcome> {
        let read = match self {
            ReadJob::Tail { source, front } => older_rows(read_log_tail(&*source, &front, stop)),
            ReadJob::Before { source, anchor } => older_rows(read_log_before(&*source, anchor)),
            ReadJob::Lookback { source, from, row } => {
                lookback_rows(read_lookback(&*source, from, &row, stop))
            }
        };
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        Some(read.unwrap_or(HistoryReadOutcome::ReadFailed))
    }

    /// Run the job and send the outcome over the channel. The sender is dropped
    /// here, so a poll on the receiver sees Disconnected once this returns. A
    /// stopped read sends nothing, and the invalidation that stopped it dropped
    /// the receiver too, so no poll ever sees that silence.
    pub(crate) fn run_and_send(self, tx: Sender<HistoryReadOutcome>, stop: &AtomicBool) {
        if let Some(outcome) = self.run(stop) {
            tx.send(outcome).ok();
        }
    }
}

/// What an older-rows read reported: rows to prepend, the end of the log, or
/// the IO failure the caller retries.
fn older_rows(
    read: Result<Option<(NamedRows, u64)>, ReadError>,
) -> Result<HistoryReadOutcome, ReadError> {
    match read {
        Ok(Some((rows, anchor))) => {
            Ok(HistoryReadOutcome::Rows(HistoryReadResult { rows, anchor }))
        }
        Ok(None) => Ok(HistoryReadOutcome::Exhausted),
        Err(ReadError::Io) => Err(ReadError::Io),
    }
}

/// What a lookback walk reported: the rows reaching the row it was sent for,
/// the end of the log without it, or a walk that spent its budget.
fn lookback_rows(read: Result<LookbackEnd, ReadError>) -> Result<HistoryReadOutcome, ReadError> {
    match read {
        Ok(LookbackEnd::Reached(rows, anchor)) => {
            Ok(HistoryReadOutcome::Rows(HistoryReadResult { rows, anchor }))
        }
        Ok(LookbackEnd::Absent) => Ok(HistoryReadOutcome::Exhausted),
        Ok(LookbackEnd::OutOfBudget) => Ok(HistoryReadOutcome::LookbackMissed),
        Err(ReadError::Io) => Err(ReadError::Io),
    }
}

/// What a lookback walk ended with.
enum LookbackEnd {
    /// The rows reaching the row the walk was sent for, and the byte offset
    /// they begin at.
    Reached(NamedRows, u64),
    /// The walk read to the log's start without the row: the log does not hold
    /// it, so no later read will find it there either.
    Absent,
    /// The walk spent its byte budget with the log still holding older rows.
    OutOfBudget,
}

/// Read older windows until the window holding this row is reached, and report
/// every window the walk crossed as one block of rows.
///
/// The first step is the read an ordinary scroll back runs from the same
/// state, so the bytes it crosses are not this walk's to count: a viewport
/// whose row a pass released stands where a scroll back to the front would
/// have stood. Each step after it reads one window older, looks through its
/// rows for the identity once, and keeps the window while the row is still
/// further back. Every window the walk crossed is handed back together, so the
/// viewport is resolved once rather than once per window. The stop flag is
/// checked between windows, so a read nobody wants any more quits after the
/// window it is inside.
fn read_lookback(
    source: &dyn TranscriptSnapshot,
    from: LookbackFrom,
    row: &RowId,
    stop: &AtomicBool,
) -> Result<LookbackEnd, ReadError> {
    let first = match from {
        LookbackFrom::Visible(front) => read_log_tail(source, &front, stop)?,
        LookbackFrom::Offset(anchor) => read_log_before(source, anchor)?,
    };
    // No window joined the rows the view holds, so there is no run of older
    // rows for the parked row to sit in. The walk reports the log's end, which
    // is what the read from the same state reports.
    let Some((first, anchor)) = first else {
        return Ok(LookbackEnd::Absent);
    };
    // Newest window first; the result is assembled oldest first at the end.
    let mut windows = vec![first];
    let mut anchor = anchor;
    if window_holds(&windows[0], row) {
        return Ok(assemble(windows, anchor));
    }
    let mut walked: u64 = 0;
    loop {
        if anchor == 0 {
            return Ok(LookbackEnd::Absent);
        }
        if stop.load(Ordering::Relaxed) {
            // The walk was abandoned. The invalidation that raised the flag
            // dropped the record holding the result, so run reports nothing
            // and this walk ends here either way.
            return Ok(LookbackEnd::OutOfBudget);
        }
        if walked >= LOOKBACK_MAX_BYTES {
            return Ok(LookbackEnd::OutOfBudget);
        }
        let Some((older, start)) = read_log_before(source, anchor)? else {
            return Ok(LookbackEnd::Absent);
        };
        // A window that does not begin below the anchor would leave the walk
        // standing still and counting no bytes; read_log_before answers None
        // for one, so the step always moves.
        walked += anchor - start;
        anchor = start;
        let found = window_holds(&older, row);
        windows.push(older);
        if found {
            return Ok(assemble(windows, anchor));
        }
    }
}

/// Whether one window's rows carry this identity. A window holds its rows in
/// order, so one pass over their names answers it.
fn window_holds(window: &NamedRows, row: &RowId) -> bool {
    window.ids().iter().any(|id| id == row)
}

/// The windows a walk crossed as one block of rows, oldest first, with the
/// byte offset the block begins at.
fn assemble(windows: Vec<NamedRows>, oldest_start: u64) -> LookbackEnd {
    let mut rows = NamedRows::default();
    for window in windows.into_iter().rev() {
        rows.append(&window);
    }
    LookbackEnd::Reached(rows, oldest_start)
}

/// What one walk of the log for one row of the view's front ended with.
enum WalkEnd {
    /// The row was met, and the rows older than it begin at this byte offset.
    Anchored(NamedRows, u64),
    /// The walk reached the log's start without meeting the row: the log
    /// holds no row that this view row stands for, so the row is no place to
    /// begin at.
    Absent,
    /// The walk met the row and then reached the log's start: the view
    /// already shows the oldest rows the log holds. A later row of the front
    /// is no better a place to begin at — the rows between the two are on
    /// view, and reading from the later row hands them back.
    Exhausted,
    /// The walk stopped before the log's start, its budget spent or the read
    /// stopped. The row is not settled either way, and what the walk read is
    /// a prefix of an answer about a log it never finished.
    Stopped,
}

/// Read the rows the log holds older than the row the visible transcript
/// starts at. The read walks back from the log tail one window at a time,
/// since the row it looks for sits behind the resident window and the bytes
/// crossed to reach it must not be held at once. The first window carrying
/// that row ends the walk, and the rows older than it in that window are the
/// result.
///
/// The view's opening rows are tried in turn as the row to begin at. A view
/// whose front was cut shows rows the log has no counterpart for, since they
/// stand in for frames the window no longer carries, and a read that can only
/// begin at such a row is a read that never begins: every scroll back would
/// walk the whole log and report its end, closing the front for good. Prove
/// the row is not in the log and the row after it is tried, so the view walks
/// back from the first row it still shares with the log. The bytes the tries
/// cross are counted together, so the tries spend one walk's budget between
/// them rather than one walk's budget each: a try that reaches the log's start
/// leaves a later try only what is left of the budget, and a row the remainder
/// does not reach stays unsettled, which is what a single walk reported there.
/// The tries widen which row the walk may begin at, not how far it travels.
///
/// None when no row of the view's front begins the older rows the log holds,
/// and when a walk stopped before it could settle that.
fn read_log_tail(
    source: &dyn TranscriptSnapshot,
    front: &[FrontRow],
    stop: &AtomicBool,
) -> Result<Option<(NamedRows, u64)>, ReadError> {
    let mut walked: u64 = 0;
    for from in 0..front.len() {
        match walk_tail_from(source, &front[from..], stop, &mut walked)? {
            WalkEnd::Anchored(rows, start) => return Ok(Some((rows, start))),
            WalkEnd::Absent => continue,
            // The two ends that are not a place to begin at: the log holds no
            // row older than one of them, and from the other the walk never
            // settled. Neither leaves a later row of the front a better one.
            WalkEnd::Exhausted | WalkEnd::Stopped => return Ok(None),
        }
    }
    Ok(None)
}

/// Walk back from the log tail for the row the rows of the view's front begin
/// at, whose run of agreeing rows decides the cut. The bytes a try crosses are
/// added to walked as it crosses them, so a try that follows starts with what
/// the tries before it spent.
fn walk_tail_from(
    source: &dyn TranscriptSnapshot,
    front: &[FrontRow],
    stop: &AtomicBool,
    walked: &mut u64,
) -> Result<WalkEnd, ReadError> {
    let mut anchor = source.log_size();
    // The rows of the window read before this one that sit just after this
    // window's newest row: an overlap whose run reaches this window's end
    // continues into them.
    let mut newer: Vec<FrontRow> = Vec::new();
    // Set once the walk has passed the overlap row itself, so the next window's
    // rows are all older than it and are the whole result.
    let mut past_overlap = false;
    while *walked < LOG_WALK_MAX_BYTES && anchor > 0 {
        // Between windows, not inside one: the read below blocks in the kernel
        // and this thread cannot interrupt it, so the flag bounds how many more
        // windows the walk spends and one is the worst case.
        if stop.load(Ordering::Relaxed) {
            return Ok(WalkEnd::Stopped);
        }
        let window = source.window_before(anchor, LOG_WINDOW_BYTES)?;
        // A window that does not end where it was asked to would leave a gap
        // between it and the rows already read, so the walk stops instead. One
        // that does not begin below the anchor would leave the walk standing
        // still, so it stops too.
        if window.rows.is_empty() || window.next_offset != anchor {
            return Ok(WalkEnd::Stopped);
        }
        if window.start_offset >= anchor {
            return Ok(WalkEnd::Stopped);
        }
        *walked += anchor - window.start_offset;
        if past_overlap {
            let start = window.start_offset;
            return Ok(WalkEnd::Anchored(window.rows, start));
        }
        let rows = overlap_rows(&window);
        if let Some(cut) = overlap_cut(&rows, &newer, front) {
            if cut > 0 {
                let start = window.start_offset;
                let mut kept = window.rows;
                kept.truncate(cut);
                return Ok(WalkEnd::Anchored(kept, start));
            }
            past_overlap = true;
        }
        newer = rows.iter().take(OVERLAP_FRONT_ROWS).cloned().collect();
        anchor = window.start_offset;
    }
    if anchor == 0 {
        // Which of the two the walk reached the start with is the whole
        // difference between a row the log never held and the log's own oldest
        // row, and the cut above is what tells them apart: a cut at the
        // window's first row is the row itself, met.
        return Ok(if past_overlap {
            WalkEnd::Exhausted
        } else {
            WalkEnd::Absent
        });
    }
    Ok(WalkEnd::Stopped)
}

/// Read the rows the log holds older than a byte offset, which the previous
/// window's own start gives. None when the log holds nothing older, or when
/// the window does not end at that offset or does not begin below it — the
/// next read starts where this one did, which would read the same rows again.
/// An Err is an IO failure the caller retries, not the log's end.
fn read_log_before(
    source: &dyn TranscriptSnapshot,
    anchor: u64,
) -> Result<Option<(NamedRows, u64)>, ReadError> {
    let window = source.window_before(anchor, LOG_WINDOW_BYTES)?;
    if window.rows.is_empty() || window.next_offset != anchor || window.start_offset >= anchor {
        return Ok(None);
    }
    let start = window.start_offset;
    Ok(Some((window.rows, start)))
}

/// The rows of one read window as the overlap match sees them: the identity
/// the log named each row with, and the rendered text the match falls back to
/// when either side carries no durable identity.
fn overlap_rows(window: &WindowLoad) -> Vec<FrontRow> {
    window
        .rows
        .lines()
        .iter()
        .zip(window.rows.ids())
        .map(|(line, id)| FrontRow {
            id: id.clone(),
            text: line.render(),
        })
        .collect()
}

/// How many of the rows read from the log to keep: the ones older than the row
/// the visible transcript starts at. Keeping the rest would print rows the
/// view already shows. The match is on that row alone, since a match further
/// into the view says nothing about where the view begins. A row both sides
/// name by a durable identity matches on identity alone, so a row text the
/// log repeats cannot match early; the match must hold for the rows after it,
/// into the rows just after this window, and the longest run wins, the newest
/// occurrence when runs tie. None when the window does not hold the row.
fn overlap_cut(loaded: &[FrontRow], newer: &[FrontRow], front: &[FrontRow]) -> Option<usize> {
    let row = front.first()?;
    let mut best: Option<(usize, usize)> = None;
    for (j, candidate) in loaded.iter().enumerate() {
        if !row_agrees(row, candidate) {
            continue;
        }
        let run = front
            .iter()
            .zip(loaded[j..].iter().chain(newer))
            .take_while(|(want, got)| row_agrees(want, got))
            .count();
        if run >= OVERLAP_RUN_ROWS && best.is_none_or(|(best_run, _)| run >= best_run) {
            best = Some((run, j));
        }
    }
    best.map(|(_, cut)| cut)
}

/// Whether a row read from the log is the row the visible front names.
/// Identity decides when both sides carry a durable one — the same text can
/// name different turns, and the entry, not the window, says which row it is.
/// Any other pair falls back to rendered text: a local identity names a
/// position in one frame log and means nothing in another read.
fn row_agrees(front: &FrontRow, read: &FrontRow) -> bool {
    if front.id.is_durable() && read.id.is_durable() {
        front.id == read.id
    } else {
        front.text == read.text
    }
}

#[cfg(test)]
mod tests;
