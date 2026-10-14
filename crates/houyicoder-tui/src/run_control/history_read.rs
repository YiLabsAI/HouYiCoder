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

use crate::records::TranscriptLine;
use crate::scroll::WINDOW_MAX_BYTES;
use crate::state::history_read::{HistoryReadOutcome, HistoryReadResult};
use crate::transcript::snapshot::TranscriptSnapshot;
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
        front: Vec<String>,
    },
    Before {
        source: Arc<dyn TranscriptSnapshot>,
        anchor: u64,
    },
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
            ReadJob::Tail { source, front } => read_log_tail(&*source, &front, stop),
            ReadJob::Before { source, anchor } => read_log_before(&*source, anchor),
        };
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        Some(match read {
            Some((rows, anchor)) => HistoryReadOutcome::Rows(HistoryReadResult { rows, anchor }),
            None => HistoryReadOutcome::Exhausted,
        })
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

/// Read the rows the log holds older than the row the visible transcript
/// starts at. The read walks back from the log tail one window at a time,
/// since the row it looks for sits behind the resident window and the bytes
/// crossed to reach it must not be held at once. The first window carrying
/// that row ends the walk, and the rows older than it in that window are the
/// result. None when no window within the walk carries it, when it is the
/// oldest row the walk reached, or when the stop flag was raised, which ends
/// the walk at the window it is inside.
fn read_log_tail(
    source: &dyn TranscriptSnapshot,
    front: &[String],
    stop: &AtomicBool,
) -> Option<(Vec<TranscriptLine>, u64)> {
    let mut anchor = source.log_size();
    let mut walked: u64 = 0;
    // The rows of the window read before this one that sit just after this
    // window's newest row: an overlap whose run reaches this window's end
    // continues into them.
    let mut newer: Vec<TranscriptLine> = Vec::new();
    // Set once the walk has passed the overlap row itself, so the next window's
    // rows are all older than it and are the whole result.
    let mut past_overlap = false;
    while walked < LOG_WALK_MAX_BYTES && anchor > 0 {
        // Between windows, not inside one: the read below blocks in the kernel
        // and this thread cannot interrupt it, so the flag bounds how many more
        // windows the walk spends and one is the worst case.
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        let window = source.window_before(anchor, LOG_WINDOW_BYTES);
        // A window that does not end where it was asked to would leave a gap
        // between it and the rows already read, so the walk stops instead. One
        // that does not begin below the anchor would leave the walk standing
        // still, so it stops too.
        if window.lines.is_empty() || window.next_offset != anchor {
            return None;
        }
        if window.start_offset >= anchor {
            return None;
        }
        walked += anchor - window.start_offset;
        if past_overlap {
            return Some((window.lines, window.start_offset));
        }
        if let Some(cut) = overlap_cut(&window.lines, &newer, front) {
            if cut > 0 {
                let mut rows = window.lines;
                rows.truncate(cut);
                return Some((rows, window.start_offset));
            }
            past_overlap = true;
        }
        newer = window
            .lines
            .iter()
            .take(OVERLAP_FRONT_ROWS)
            .cloned()
            .collect();
        anchor = window.start_offset;
    }
    None
}

/// Read the rows the log holds older than a byte offset, which the previous
/// window's own start gives. None when the log holds nothing older, or when
/// the window does not end at that offset or does not begin below it — the
/// next read starts where this one did, which would read the same rows again.
fn read_log_before(
    source: &dyn TranscriptSnapshot,
    anchor: u64,
) -> Option<(Vec<TranscriptLine>, u64)> {
    let window = source.window_before(anchor, LOG_WINDOW_BYTES);
    if window.lines.is_empty() || window.next_offset != anchor || window.start_offset >= anchor {
        return None;
    }
    Some((window.lines, window.start_offset))
}

/// How many of the rows read from the log to keep: the ones older than the row
/// the visible transcript starts at. Keeping the rest would print rows the
/// view already shows. The match is on that row alone, since a match further
/// into the view says nothing about where the view begins. A row text the log
/// repeats matches early, so the match must hold for the rows after it, into
/// the rows just after this window, and the longest run wins. None when the
/// window does not hold the row.
fn overlap_cut(
    loaded: &[TranscriptLine],
    newer: &[TranscriptLine],
    front: &[String],
) -> Option<usize> {
    let row = front.first()?;
    let mut texts: Vec<String> = loaded.iter().map(|line| line.render()).collect();
    texts.extend(newer.iter().map(|line| line.render()));
    let mut best: Option<(usize, usize)> = None;
    for (j, text) in texts.iter().enumerate() {
        if j >= loaded.len() {
            break;
        }
        if text != row {
            continue;
        }
        let run = front
            .iter()
            .zip(&texts[j..])
            .take_while(|(want, got)| want == got)
            .count();
        if run >= OVERLAP_RUN_ROWS && best.is_none_or(|(best_run, _)| run > best_run) {
            best = Some((run, j));
        }
    }
    best.map(|(_, cut)| cut)
}

#[cfg(test)]
mod tests;
