use super::*;
use crate::records::TranscriptLine;
use crate::row_id::RowId;
use crate::state::history_read::HistoryReadPoll;
use crate::state::history_read::HistoryReads;
use crate::state::transcript::DiskFront;
use crate::transcript::snapshot::ScanProgress;
use crate::transcript::snapshot::{NoTranscriptSnapshot, ReadError, SnapshotLoad, WindowLoad};
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;

/// One row of a view front with no durable identity, so the overlap match
/// falls back to rendered text.
fn text_front(text: &str) -> FrontRow {
    FrontRow {
        id: RowId::local(0),
        text: text.to_string(),
    }
}

/// A read window whose rows carry durable identities: one event name and one
/// user row text per entry, in order.
fn durable_window(rows: &[(&str, &str)], start_offset: u64, next_offset: u64) -> WindowLoad {
    WindowLoad {
        rows: NamedRows::from_parts(
            rows.iter()
                .map(|(_, text)| TranscriptLine::User((*text).into()))
                .collect(),
            rows.iter().map(|(event, _)| RowId::from(*event)).collect(),
        ),
        start_offset,
        next_offset,
        skipped: 0,
        bytes_total: next_offset,
    }
}

/// A view front whose rows carry durable identities: one event name and one
/// user row text per entry, in order.
fn durable_front(rows: &[(&str, &str)]) -> Vec<FrontRow> {
    rows.iter()
        .map(|(event, text)| FrontRow {
            id: RowId::from(*event),
            text: TranscriptLine::User((*text).into()).render(),
        })
        .collect()
}

/// The rendered texts of a read's rows, oldest first.
fn rendered(rows: &NamedRows) -> Vec<String> {
    rows.lines().iter().map(|line| line.render()).collect()
}

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
    fn tail_window(&self, max_bytes: u64) -> Result<WindowLoad, ReadError> {
        self.empty.tail_window(max_bytes)
    }
    fn window_before(&self, _from_byte: u64, _max_bytes: u64) -> Result<WindowLoad, ReadError> {
        Ok(self.window.clone())
    }
    fn scan_chunk(&self) -> ScanProgress {
        self.empty.scan_chunk()
    }
    fn byte_at(&self, event_idx: usize) -> Option<u64> {
        self.empty.byte_at(event_idx)
    }
    fn event_count(&self) -> Option<usize> {
        self.empty.event_count()
    }
}

fn window_ending_at(next_offset: u64) -> FixedWindow {
    let rows = NamedRows::from_lines(vec![TranscriptLine::User("older".into())]);
    FixedWindow {
        empty: NoTranscriptSnapshot,
        window: WindowLoad {
            rows,
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
    assert!(source.window(0, 1024).rows.is_empty(), "no forward window");
    assert!(
        source.tail_window(1024).unwrap().rows.is_empty(),
        "no tail window"
    );
    assert!(!source.scan_chunk().done, "no index");
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
        read_log_before(&source, 5000).unwrap().is_none(),
        "a window ending past the anchor is refused"
    );
}

/// The same window is taken when it does end at the anchor, so the refusal
/// above is the mismatch and not the rows.
#[test]
fn test_before_takes_matching_window() {
    let source = window_ending_at(5000);
    assert!(
        read_log_before(&source, 5000).unwrap().is_some(),
        "a window ending at the anchor is the one the walk takes"
    );
}

/// A source that hands back one window of a fixed step per call, counts the
/// calls, and invalidates the read owner on a chosen one.
///
/// The count is the event a stop test waits on, so nothing here sleeps and
/// nothing races. Invalidating from inside a window read models the worst
/// place an invalidation can land on a real worker: partway through a call
/// that cannot be interrupted, which is why the bound is measured in
/// windows the source was asked for rather than in elapsed time.
struct SteppingWindow {
    empty: NoTranscriptSnapshot,
    reads: Arc<Mutex<HistoryReads>>,
    log_size: u64,
    step: u64,
    /// The call number that invalidates, or none for a walk nobody stops.
    stop_at: Option<u64>,
    calls: AtomicU64,
}

impl SteppingWindow {
    fn new(
        reads: Arc<Mutex<HistoryReads>>,
        log_size: u64,
        step: u64,
        stop_at: Option<u64>,
    ) -> Self {
        Self {
            empty: NoTranscriptSnapshot,
            reads,
            log_size,
            step,
            stop_at,
            calls: AtomicU64::new(0),
        }
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}

/// This double models the older window only; everything else answers as a
/// source with no log does, which is what the delegations say.
impl TranscriptSnapshot for SteppingWindow {
    fn log_size(&self) -> u64 {
        self.log_size
    }
    fn load(&self, max_bytes: u64) -> SnapshotLoad {
        self.empty.load(max_bytes)
    }
    fn window(&self, anchor: u64, max_bytes: u64) -> WindowLoad {
        self.empty.window(anchor, max_bytes)
    }
    fn tail_window(&self, max_bytes: u64) -> Result<WindowLoad, ReadError> {
        self.empty.tail_window(max_bytes)
    }
    fn window_before(&self, from_byte: u64, _max_bytes: u64) -> Result<WindowLoad, ReadError> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        if self.stop_at == Some(call) {
            self.reads
                .lock()
                .expect("the owner is not poisoned")
                .invalidate();
        }
        let start = from_byte.saturating_sub(self.step);
        let rows = NamedRows::from_lines(vec![TranscriptLine::User(format!("row at {start}"))]);
        Ok(WindowLoad {
            rows,
            start_offset: start,
            next_offset: from_byte,
            skipped: 0,
            bytes_total: self.log_size,
        })
    }
    fn scan_chunk(&self) -> ScanProgress {
        self.empty.scan_chunk()
    }
    fn byte_at(&self, event_idx: usize) -> Option<u64> {
        self.empty.byte_at(event_idx)
    }
    fn event_count(&self) -> Option<usize> {
        self.empty.event_count()
    }
}

/// A log of five thousand bytes read a thousand at a time, whose rows never
/// match the view's front, so a walk over it runs to the start and takes
/// five windows to get there.
fn five_window_log(reads: Arc<Mutex<HistoryReads>>, stop_at: Option<u64>) -> Arc<SteppingWindow> {
    Arc::new(SteppingWindow::new(reads, 5000, 1000, stop_at))
}

/// This double models the older window and the log's size; everything else
/// answers as a source with no log does, which is what the delegations say.
#[test]
fn test_stepping_window_older_only() {
    let reads = Arc::new(Mutex::new(HistoryReads::default()));
    let source = five_window_log(reads, None);
    assert_eq!(source.log_size(), 5000, "the size the walk starts from");
    assert!(source.load(1024).lines.is_empty(), "no whole-log load");
    assert!(source.window(0, 1024).rows.is_empty(), "no forward window");
    assert!(
        source.tail_window(1024).unwrap().rows.is_empty(),
        "no tail window"
    );
    assert!(!source.scan_chunk().done, "no index");
    assert!(source.byte_at(0).is_none(), "no offsets");
    assert!(source.event_count().is_none(), "no event count");
}

/// A read invalidated partway through its walk quits at the window it is
/// inside and reports nothing.
///
/// Both halves of the contract are driven together because each alone
/// proves too little: an owner that raises a flag is worthless if the walk
/// watches a different one, and a walk that honors a flag is worthless if
/// nothing raises it. The flag comes out of the dispatch and the raise goes
/// through the owner that dispatched it, so a wiring that handed the task
/// any other flag walks all five windows and fails.
///
/// Reporting nothing is the other half. The rows a stopped walk holds are a
/// prefix of an answer about a log it never finished reading, and reporting
/// them as no older row would latch the disk front closed on a view that
/// has older rows to show.
#[test]
fn test_invalidate_stops_walk() {
    let reads = Arc::new(Mutex::new(HistoryReads::default()));
    let source = five_window_log(Arc::clone(&reads), Some(2));
    let snapshot: Arc<dyn TranscriptSnapshot> = source.clone();
    let (_tx, rx) = mpsc::channel::<HistoryReadOutcome>();
    let lease =
        reads
            .lock()
            .expect("the owner is not poisoned")
            .dispatch(0, DiskFront::Unloaded, rx);

    let job = ReadJob::Tail {
        source: snapshot,
        front: vec![text_front("the row the view starts at")],
    };
    let outcome = job.run(lease.stop());

    assert!(outcome.is_none(), "a stopped walk reports nothing");
    assert_eq!(
        source.calls(),
        2,
        "the walk quit at the window it was inside, not at its byte budget"
    );
}

/// The same walk with nobody stopping it runs to the log's start and says
/// so. Five windows rather than two is the difference the stop test turns
/// on, and no older row rather than nothing at all is the outcome a stopped
/// walk must not be mistaken for.
#[test]
fn test_walk_reaches_start() {
    let reads = Arc::new(Mutex::new(HistoryReads::default()));
    let source = five_window_log(Arc::clone(&reads), None);
    let snapshot: Arc<dyn TranscriptSnapshot> = source.clone();
    let (_tx, rx) = mpsc::channel::<HistoryReadOutcome>();
    let lease =
        reads
            .lock()
            .expect("the owner is not poisoned")
            .dispatch(0, DiskFront::Unloaded, rx);

    let job = ReadJob::Tail {
        source: snapshot,
        front: vec![text_front("the row the view starts at")],
    };
    let outcome = job.run(lease.stop());

    assert!(
        matches!(outcome, Some(HistoryReadOutcome::Exhausted)),
        "a completed walk reports that the log ran out: {outcome:?}"
    );
    assert_eq!(source.calls(), 5, "the walk read the whole log");
}

/// Answers each older-window read with the scripted window ending at the
/// offset the walk asked about.
struct ScriptedWindows {
    empty: NoTranscriptSnapshot,
    windows: Vec<WindowLoad>,
}

impl TranscriptSnapshot for ScriptedWindows {
    fn log_size(&self) -> u64 {
        self.windows.first().map_or(0, |w| w.next_offset)
    }
    fn load(&self, max_bytes: u64) -> SnapshotLoad {
        self.empty.load(max_bytes)
    }
    fn window(&self, anchor: u64, max_bytes: u64) -> WindowLoad {
        self.empty.window(anchor, max_bytes)
    }
    fn tail_window(&self, max_bytes: u64) -> Result<WindowLoad, ReadError> {
        self.empty.tail_window(max_bytes)
    }
    fn window_before(&self, from_byte: u64, _max_bytes: u64) -> Result<WindowLoad, ReadError> {
        Ok(self
            .windows
            .iter()
            .find(|w| w.next_offset == from_byte)
            .cloned()
            .unwrap_or_default())
    }
    fn scan_chunk(&self) -> ScanProgress {
        self.empty.scan_chunk()
    }
    fn byte_at(&self, event_idx: usize) -> Option<u64> {
        self.empty.byte_at(event_idx)
    }
    fn event_count(&self) -> Option<usize> {
        self.empty.event_count()
    }
}

/// The first window a tail walk reads can hold nothing older than the front's
/// oldest row, so the walk continues and returns the next window whole.
#[test]
fn test_tail_walks_past_overlap() {
    let overlap_rows = NamedRows::from_lines(vec![
        TranscriptLine::User("A".into()),
        TranscriptLine::User("B".into()),
        TranscriptLine::User("C".into()),
    ]);
    let overlap_window = WindowLoad {
        rows: overlap_rows,
        start_offset: 2000,
        next_offset: 3000,
        skipped: 0,
        bytes_total: 3000,
    };
    let older = WindowLoad {
        rows: NamedRows::from_lines(vec![TranscriptLine::User("D".into())]),
        start_offset: 1000,
        next_offset: 2000,
        skipped: 0,
        bytes_total: 3000,
    };
    let source: Arc<dyn TranscriptSnapshot> = Arc::new(ScriptedWindows {
        empty: NoTranscriptSnapshot,
        windows: vec![overlap_window, older],
    });
    let job = ReadJob::Tail {
        source,
        front: vec![
            text_front(&TranscriptLine::User("A".into()).render()),
            text_front(&TranscriptLine::User("B".into()).render()),
            text_front(&TranscriptLine::User("C".into()).render()),
        ],
    };
    let stop = AtomicBool::new(false);
    match job.run(&stop) {
        Some(HistoryReadOutcome::Rows(result)) => {
            assert_eq!(result.anchor, 1000, "the older window's start anchors it");
            assert_eq!(result.rows.len(), 1, "the whole older window is the answer");
        }
        other => panic!("the walk returned the window below the overlap: {other:?}"),
    }
}

/// A row text the log repeats cannot cut the window at its earliest copy: the
/// front's rows carry the identities of the newest occurrence, and the cut
/// keeps the rows older than that occurrence. A text-only match cuts at the
/// earliest copy and drops the rows between the two.
#[test]
fn test_overlap_picks_newest_repeat() {
    let window = durable_window(
        &[
            ("e0", "A"),
            ("e1", "B"),
            ("e2", "C"),
            ("e3", "X"),
            ("e4", "A"),
            ("e5", "B"),
            ("e6", "C"),
        ],
        1000,
        2000,
    );
    let source: Arc<dyn TranscriptSnapshot> = Arc::new(ScriptedWindows {
        empty: NoTranscriptSnapshot,
        windows: vec![window],
    });
    let job = ReadJob::Tail {
        source,
        front: durable_front(&[("e4", "A"), ("e5", "B"), ("e6", "C")]),
    };
    let stop = AtomicBool::new(false);
    match job.run(&stop) {
        Some(HistoryReadOutcome::Rows(result)) => {
            assert_eq!(result.anchor, 1000, "the window's start anchors it");
            assert_eq!(
                rendered(&result.rows),
                vec!["> A", "> B", "> C", "> X"],
                "the rows older than the newest occurrence are the answer"
            );
            assert_eq!(
                result.rows.ids(),
                vec![
                    RowId::from("e0"),
                    RowId::from("e1"),
                    RowId::from("e2"),
                    RowId::from("e3"),
                ],
                "the kept rows keep their own identities"
            );
        }
        other => panic!("the walk cut at the newest occurrence: {other:?}"),
    }
}

/// An overlap that reaches the end of one window continues into the rows the
/// walk already read from the window above it, so a front whose first rows sit
/// at the older window's newest rows still cuts there. A match confined to one
/// window would cut at the earlier copy of the front's text and hand back rows
/// the view already shows.
#[test]
fn test_overlap_joins_across_windows() {
    let older = durable_window(
        &[
            ("e0", "A"),
            ("e1", "B"),
            ("e2", "C"),
            ("e3", "X"),
            ("e4", "A"),
        ],
        1000,
        2000,
    );
    let newer = durable_window(&[("e5", "B"), ("e6", "C")], 2000, 3000);
    let source: Arc<dyn TranscriptSnapshot> = Arc::new(ScriptedWindows {
        empty: NoTranscriptSnapshot,
        windows: vec![newer, older],
    });
    let job = ReadJob::Tail {
        source,
        front: durable_front(&[("e4", "A"), ("e5", "B"), ("e6", "C")]),
    };
    let stop = AtomicBool::new(false);
    match job.run(&stop) {
        Some(HistoryReadOutcome::Rows(result)) => {
            assert_eq!(result.anchor, 1000, "the older window's start anchors it");
            assert_eq!(
                rendered(&result.rows),
                vec!["> A", "> B", "> C", "> X"],
                "the rows older than the front's first row are the answer"
            );
        }
        other => panic!("the walk joined the windows at the front's rows: {other:?}"),
    }
}

/// Rows whose text agrees with the front's but whose identities name other
/// events do not match: the entry, not the window, says which row it is, so
/// the walk refuses the lookalike and reports nothing older rather than
/// splicing another turn's rows under the view's front.
#[test]
fn test_overlap_rejects_mismatched_identity() {
    let lookalike = durable_window(&[("e0", "A"), ("e1", "B"), ("e2", "C")], 1000, 2000);
    let oldest = durable_window(&[("e3", "D")], 0, 1000);
    let source: Arc<dyn TranscriptSnapshot> = Arc::new(ScriptedWindows {
        empty: NoTranscriptSnapshot,
        windows: vec![lookalike, oldest],
    });
    let job = ReadJob::Tail {
        source,
        front: durable_front(&[("f0", "A"), ("f1", "B"), ("f2", "C")]),
    };
    let stop = AtomicBool::new(false);
    match job.run(&stop) {
        Some(HistoryReadOutcome::Exhausted) => {}
        other => panic!("no window holds the front's rows: {other:?}"),
    }
}

/// A front row with no durable identity falls back to rendered text, so a
/// view raised in-process still finds its rows in a window the log named.
#[test]
fn test_local_front_matches_text() {
    let window = durable_window(&[("e0", "A"), ("e1", "B"), ("e2", "C")], 1000, 2000);
    let oldest = durable_window(&[("e3", "D")], 0, 1000);
    let source: Arc<dyn TranscriptSnapshot> = Arc::new(ScriptedWindows {
        empty: NoTranscriptSnapshot,
        windows: vec![window, oldest],
    });
    let job = ReadJob::Tail {
        source,
        front: vec![
            text_front(&TranscriptLine::User("A".into()).render()),
            text_front(&TranscriptLine::User("B".into()).render()),
            text_front(&TranscriptLine::User("C".into()).render()),
        ],
    };
    let stop = AtomicBool::new(false);
    match job.run(&stop) {
        Some(HistoryReadOutcome::Rows(result)) => {
            assert_eq!(result.anchor, 0, "the oldest window's start anchors it");
            assert_eq!(
                rendered(&result.rows),
                vec!["> D"],
                "the whole window below the overlap is the answer"
            );
        }
        other => panic!("the text fallback matched the front's rows: {other:?}"),
    }
}

/// An older-window read that either fails or comes back empty — the two
/// answers a caller must not collapse.
struct InjectedWindow {
    empty: NoTranscriptSnapshot,
    failure: Option<ReadError>,
}

impl TranscriptSnapshot for InjectedWindow {
    fn log_size(&self) -> u64 {
        5000
    }
    fn load(&self, max_bytes: u64) -> SnapshotLoad {
        self.empty.load(max_bytes)
    }
    fn window(&self, anchor: u64, max_bytes: u64) -> WindowLoad {
        self.empty.window(anchor, max_bytes)
    }
    fn tail_window(&self, max_bytes: u64) -> Result<WindowLoad, ReadError> {
        self.empty.tail_window(max_bytes)
    }
    fn window_before(&self, _from_byte: u64, _max_bytes: u64) -> Result<WindowLoad, ReadError> {
        match self.failure.clone() {
            Some(e) => Err(e),
            None => Ok(WindowLoad::default()),
        }
    }
    fn scan_chunk(&self) -> ScanProgress {
        self.empty.scan_chunk()
    }
    fn byte_at(&self, event_idx: usize) -> Option<u64> {
        self.empty.byte_at(event_idx)
    }
    fn event_count(&self) -> Option<usize> {
        self.empty.event_count()
    }
}

/// Run a tail read through the dispatch, the channel, and the owner's poll
/// the production loop uses.
fn injected_tail_read(
    failure: Option<ReadError>,
) -> (HistoryReadOutcome, Arc<Mutex<HistoryReads>>) {
    let reads = Arc::new(Mutex::new(HistoryReads::default()));
    let source: Arc<dyn TranscriptSnapshot> = Arc::new(InjectedWindow {
        empty: NoTranscriptSnapshot,
        failure,
    });
    let (tx, rx) = mpsc::channel::<HistoryReadOutcome>();
    let lease =
        reads
            .lock()
            .expect("the owner is not poisoned")
            .dispatch(0, DiskFront::Unloaded, rx);
    let job = ReadJob::Tail {
        source,
        front: vec![text_front("the row the view starts at")],
    };
    job.run_and_send(tx, lease.stop());
    // The send already happened; the poll finds it ready.
    let pending = reads
        .lock()
        .expect("the owner is not poisoned")
        .take()
        .expect("the dispatched read is on file");
    let outcome = match pending.poll() {
        HistoryReadPoll::Ready(outcome) => outcome,
        HistoryReadPoll::Pending => panic!("the worker already sent its outcome"),
        HistoryReadPoll::Disconnected => panic!("the worker left without sending"),
    };
    (outcome, reads)
}

/// A failed window read is not the log's end: the outcome reports a failed
/// read and the worker's slot is freed.
#[test]
fn test_failed_read_stays_open() {
    let (outcome, reads) = injected_tail_read(Some(ReadError::Io));
    assert!(
        matches!(outcome, HistoryReadOutcome::ReadFailed),
        "a failed window read is not the log's end: {outcome:?}"
    );
    assert_eq!(
        reads.lock().expect("the owner is not poisoned").running(),
        0,
        "the failed read's worker freed its slot"
    );
}

/// An empty window still means the log's end.
#[test]
fn test_empty_window_exhausts() {
    let (outcome, _) = injected_tail_read(None);
    assert!(
        matches!(outcome, HistoryReadOutcome::Exhausted),
        "an empty window is the log's end: {outcome:?}"
    );
}
