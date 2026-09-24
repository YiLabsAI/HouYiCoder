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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::snapshot::{NoTranscriptSnapshot, SnapshotLoad, WindowLoad};

    /// A source whose older window holds real rows but does not end where the
    /// walk asked. The walk joins each window to the rows it already read on
    /// that offset, so taking this one would print a gap.
    struct FixedWindow {
        empty: NoTranscriptSnapshot,
        window: WindowLoad,
    }

    impl TranscriptSnapshot for FixedWindow {
        fn log_size(&self) -> u64 {
            self.empty.log_size()
        }
        fn load(&self, max_bytes: u64) -> SnapshotLoad {
            self.empty.load(max_bytes)
        }
        fn window(&self, anchor: u64, max_bytes: u64) -> WindowLoad {
            self.empty.window(anchor, max_bytes)
        }
        fn tail_window(&self, max_bytes: u64) -> WindowLoad {
            self.empty.tail_window(max_bytes)
        }
        fn window_before(&self, _from_byte: u64, _max_bytes: u64) -> WindowLoad {
            self.window.clone()
        }
        fn index_chunk(&self) -> crate::transcript::snapshot::IndexProgress {
            self.empty.index_chunk()
        }
        fn byte_at(&self, event_idx: usize) -> Option<u64> {
            self.empty.byte_at(event_idx)
        }
        fn event_count(&self) -> Option<usize> {
            self.empty.event_count()
        }
    }

    fn window_ending_at(next_offset: u64) -> FixedWindow {
        FixedWindow {
            empty: NoTranscriptSnapshot,
            window: WindowLoad {
                lines: vec![TranscriptLine::User("older".into())],
                start_offset: 100,
                next_offset,
                skipped: 0,
                bytes_total: 5000,
            },
        }
    }

    /// This double models the older window only; everything else answers as a
    /// source with no log does, which is what the delegations say.
    #[test]
    fn test_fixed_window_older_only() {
        let source = window_ending_at(5000);
        assert_eq!(source.log_size(), 0, "no log size of its own");
        assert!(source.load(1024).lines.is_empty(), "no whole-log load");
        assert!(source.window(0, 1024).lines.is_empty(), "no forward window");
        assert!(source.tail_window(1024).lines.is_empty(), "no tail window");
        assert!(!source.index_chunk().done, "no index");
        assert!(source.byte_at(0).is_none(), "no offsets");
        assert!(source.event_count().is_none(), "no event count");
    }

    /// A window that ends somewhere other than the anchor is refused, however
    /// many rows it carries: joining it would leave a gap between it and the
    /// rows already read.
    #[test]
    fn test_before_refuses_gap() {
        let source = window_ending_at(5005);
        assert!(
            read_log_before(&source, 5000).is_none(),
            "a window ending past the anchor is refused"
        );
    }

    /// The same window is taken when it does end at the anchor, so the refusal
    /// above is the mismatch and not the rows.
    #[test]
    fn test_before_takes_matching_window() {
        let source = window_ending_at(5000);
        assert!(
            read_log_before(&source, 5000).is_some(),
            "a window ending at the anchor is the one the walk takes"
        );
    }
}
