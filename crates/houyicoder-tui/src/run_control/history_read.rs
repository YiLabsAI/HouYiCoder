//! The background read that brings older rows back from the session log, kept
//! apart from the rebuild file so the rebuild stays under its size floor. The
//! ReadJob is what the draw path hands to a background task; the three
//! functions are the read itself, shared by the job and unchanged from when
//! the rebuild ran them synchronously.

use std::sync::Arc;

use crate::records::TranscriptLine;
use crate::state::transcript::{HistoryReadOutcome, HistoryReadResult};
use crate::transcript::snapshot::TranscriptSnapshot;

/// Byte budget of one older-row read from the session log, and of each window
/// the first read walks back through.
const LOG_WINDOW_BYTES: u64 = crate::scroll::WINDOW_MAX_BYTES;
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
/// completion and sends the outcome over the channel the pending slot holds.
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
    fn run(self) -> HistoryReadOutcome {
        match self {
            ReadJob::Tail { source, front } => match read_log_tail(&*source, &front) {
                Some((rows, anchor)) => {
                    HistoryReadOutcome::Rows(HistoryReadResult { rows, anchor })
                }
                None => HistoryReadOutcome::Exhausted,
            },
            ReadJob::Before { source, anchor } => match read_log_before(&*source, anchor) {
                Some((rows, anchor)) => {
                    HistoryReadOutcome::Rows(HistoryReadResult { rows, anchor })
                }
                None => HistoryReadOutcome::Exhausted,
            },
        }
    }

    /// Run the job and send the outcome over the channel. The sender is dropped
    /// here, so a poll on the receiver sees Disconnected once this returns.
    pub(crate) fn run_and_send(self, tx: std::sync::mpsc::Sender<HistoryReadOutcome>) {
        tx.send(self.run()).ok();
    }
}

/// Read the rows the log holds older than the row the visible transcript
/// starts at. The read walks back from the log tail one window at a time,
/// since the row it looks for sits behind the resident window and the bytes
/// crossed to reach it must not be held at once. The first window carrying
/// that row ends the walk, and the rows older than it in that window are the
/// result. None when no window within the walk carries it, or when it is the
/// oldest row the walk reached.
fn read_log_tail(
    source: &dyn TranscriptSnapshot,
    front: &[String],
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
