//! What a search read walks and what it reports: which log shape an open
//! reads, where a step stops, and what a stopped or failed read sends.

use super::*;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::channel;

use crate::records::TranscriptLine;
use crate::test_harness::MockSnapshot;
use crate::transcript::rows::NamedRows;
use crate::transcript::scroll::SEARCH_LOG_THRESHOLD;
use crate::transcript::snapshot::{ReadError, ScanProgress, SnapshotLoad, WindowLoad};

/// A source holding nothing but the log size and the rows a whole-log load
/// returns.
fn whole_log_source(lines: Vec<TranscriptLine>, log_bytes: u64) -> Arc<dyn TranscriptSnapshot> {
    Arc::new(MockSnapshot {
        lines,
        log_bytes,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows: Vec::new(),
        scan_steps: 0,
        scan_calls: Default::default(),
    })
}

/// One window of the log, spanning the given bytes and holding the given rows.
fn window(rows: Vec<TranscriptLine>, start_offset: u64, next_offset: u64) -> WindowLoad {
    WindowLoad {
        rows: NamedRows::from_lines(rows),
        start_offset,
        next_offset,
        skipped: 0,
        bytes_total: next_offset,
    }
}

/// A source whose windows are read by the byte ranges they span.
fn windowed_source(windows: Vec<WindowLoad>, log_bytes: u64) -> Arc<dyn TranscriptSnapshot> {
    Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows,
        scan_steps: 0,
        scan_calls: Default::default(),
    })
}

/// A source whose older windows cannot be read at all.
struct UnreadableOlder;

impl TranscriptSnapshot for UnreadableOlder {
    fn log_size(&self) -> u64 {
        SEARCH_LOG_THRESHOLD + 1
    }
    fn load(&self, _max_bytes: u64) -> SnapshotLoad {
        SnapshotLoad::default()
    }
    fn window(&self, _anchor: u64, _max_bytes: u64) -> WindowLoad {
        WindowLoad::default()
    }
    fn tail_window(&self, _max_bytes: u64) -> Result<WindowLoad, ReadError> {
        Err(ReadError::Io)
    }
    fn window_before(&self, _from_byte: u64, _max_bytes: u64) -> Result<WindowLoad, ReadError> {
        Err(ReadError::Io)
    }
    fn scan_chunk(&self) -> ScanProgress {
        ScanProgress::default()
    }
    fn byte_at(&self, _event_idx: usize) -> Option<u64> {
        None
    }
    fn event_count(&self) -> Option<usize> {
        None
    }
}

fn run(job: SearchReadJob, source: &Arc<dyn TranscriptSnapshot>) -> Option<SearchReadOutcome> {
    let stop = AtomicBool::new(false);
    job.run(source, &stop)
}

/// A source whose windows are read by the byte ranges they span, counting the
/// reads so a test can say how far a walk went, and raising the read's stop
/// signal at whichever read the test names, standing in for an owner that
/// invalidates the read while the walk is inside a window.
struct CountedWindows {
    windows: Vec<WindowLoad>,
    window_reads: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    stops_at: Option<usize>,
}

impl TranscriptSnapshot for CountedWindows {
    fn log_size(&self) -> u64 {
        200
    }
    fn load(&self, _max_bytes: u64) -> SnapshotLoad {
        SnapshotLoad::default()
    }
    fn window(&self, _anchor: u64, _max_bytes: u64) -> WindowLoad {
        WindowLoad::default()
    }
    fn tail_window(&self, _max_bytes: u64) -> Result<WindowLoad, ReadError> {
        Ok(WindowLoad::default())
    }
    fn window_before(&self, from_byte: u64, _max_bytes: u64) -> Result<WindowLoad, ReadError> {
        let reads = self.window_reads.fetch_add(1, Ordering::Relaxed);
        if self.stops_at == Some(reads) {
            self.stop.store(true, Ordering::Relaxed);
        }
        Ok(self
            .windows
            .iter()
            .find(|w| w.next_offset == from_byte)
            .cloned()
            .unwrap_or_default())
    }
    fn scan_chunk(&self) -> ScanProgress {
        ScanProgress::default()
    }
    fn byte_at(&self, _event_idx: usize) -> Option<u64> {
        None
    }
    fn event_count(&self) -> Option<usize> {
        None
    }
}

/// Under the threshold the whole log is the corpus, so a match anywhere in it
/// is a match the view can jump to.
#[test]
fn test_open_reads_whole_log() {
    let source = whole_log_source(
        vec![
            TranscriptLine::User("a needle here".into()),
            TranscriptLine::Agent("nothing".into()),
        ],
        1024,
    );

    let outcome = run(SearchReadJob::Open, &source).expect("the read was not stopped");

    let SearchReadOutcome::Under(load) = outcome else {
        panic!("a log under the threshold is read whole, not windowed");
    };
    assert_eq!(load.rows.len(), 2, "the whole log came back as its rows");
}

/// Over the threshold the newest window is the corpus, so the view opens on the
/// most recent part of the log rather than loading all of it.
#[test]
fn test_open_reads_newest_window() {
    let log_bytes = SEARCH_LOG_THRESHOLD + 1;
    let source = windowed_source(
        vec![window(
            vec![TranscriptLine::Agent("the newest rows".into())],
            4096,
            log_bytes,
        )],
        log_bytes,
    );

    let outcome = run(SearchReadJob::Open, &source).expect("the read was not stopped");

    let SearchReadOutcome::Over {
        log_bytes: seen,
        window,
    } = outcome
    else {
        panic!("a log over the threshold is read as its newest window");
    };
    assert_eq!(
        seen, log_bytes,
        "the chrome reads the log size it was frozen at"
    );
    assert_eq!(window.start_offset, 4096, "the window read is the tail one");
}

/// A step walks past windows holding no match and lands on the first one that
/// does, which is the closest match in the direction walked.
#[test]
fn test_walk_lands_on_match() {
    let source = windowed_source(
        vec![
            window(vec![TranscriptLine::Agent("older rows".into())], 0, 100),
            window(vec![TranscriptLine::User("a needle".into())], 100, 200),
            window(vec![TranscriptLine::Agent("newer rows".into())], 200, 300),
        ],
        300,
    );

    let outcome = run(
        SearchReadJob::Step {
            from_byte: 300,
            toward: WalkToward::Older,
            file_size: 300,
            query: "needle".into(),
        },
        &source,
    )
    .expect("the read was not stopped");

    let SearchReadOutcome::Stepped(window) = outcome else {
        panic!("a window holding a match is a window to land on");
    };
    assert_eq!(
        window.start_offset, 100,
        "the walk stopped at the window holding the match, not at the nearest one"
    );
}

/// A walk with no match in the log reports no step rather than a window: the
/// view stays where it is, and the absence is not read as the log's end.
#[test]
fn test_walk_stops_at_start() {
    let source = windowed_source(
        vec![
            window(vec![TranscriptLine::Agent("older rows".into())], 0, 100),
            window(vec![TranscriptLine::Agent("newer rows".into())], 100, 200),
        ],
        200,
    );

    let outcome = run(
        SearchReadJob::Step {
            from_byte: 200,
            toward: WalkToward::Older,
            file_size: 200,
            query: "needle".into(),
        },
        &source,
    )
    .expect("the read was not stopped");

    assert!(
        matches!(outcome, SearchReadOutcome::NoStep),
        "no window holds a match, so there is no window to land on"
    );
}

/// A stopped walk reports nothing at all, because the record that dispatched it
/// is gone: an outcome sent now would sit in a channel nobody holds.
#[test]
fn test_stopped_walk_sends_nothing() {
    let source = windowed_source(
        vec![window(
            vec![TranscriptLine::User("a needle".into())],
            0,
            100,
        )],
        100,
    );
    let stop = AtomicBool::new(true);

    let outcome = SearchReadJob::Step {
        from_byte: 100,
        toward: WalkToward::Older,
        file_size: 100,
        query: "needle".into(),
    }
    .run(&source, &stop);

    assert!(outcome.is_none(), "a stopped read reports no outcome");
}

/// A walk told to stop while it is inside a window stops at the next window
/// boundary rather than walking on: the window holding the match is never read,
/// and nothing is sent for a record that is gone.
#[test]
fn test_stop_between_windows() {
    let stop = Arc::new(AtomicBool::new(false));
    let window_reads = Arc::new(AtomicUsize::new(0));
    let source: Arc<dyn TranscriptSnapshot> = Arc::new(CountedWindows {
        stop: Arc::clone(&stop),
        stops_at: Some(0),
        windows: vec![
            window(vec![TranscriptLine::User("a needle".into())], 0, 100),
            window(
                vec![TranscriptLine::Agent("no match here".into())],
                100,
                200,
            ),
        ],
        window_reads: Arc::clone(&window_reads),
    });

    let outcome = SearchReadJob::Step {
        from_byte: 200,
        toward: WalkToward::Older,
        file_size: 200,
        query: "needle".into(),
    }
    .run(&source, &stop);

    assert!(outcome.is_none(), "a stopped walk reports no outcome");
    assert_eq!(
        window_reads.load(Ordering::Relaxed),
        1,
        "the walk stopped at the boundary instead of reading the window beyond it"
    );
}

/// A walk that spends its window bound reports no step even though the log holds
/// a match past the bound: the walk stopped at its own limit, so the absence is
/// not read as the log's end.
#[test]
fn test_walk_spends_bound() {
    // The log is longer than the bound by a wide margin, so the walk's own limit
    // rather than the end of the log is what stops it.
    let window_count = 4 * SEARCH_WALK_MAX_WINDOWS;
    let windows = (0..window_count)
        .map(|i| {
            let rows = if i == 0 {
                vec![TranscriptLine::User("a needle".into())]
            } else {
                vec![TranscriptLine::Agent(format!("rows {i}"))]
            };
            window(rows, i as u64 * 100, (i as u64 + 1) * 100)
        })
        .collect();
    let window_reads = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let file_size = window_count as u64 * 100;
    let source: Arc<dyn TranscriptSnapshot> = Arc::new(CountedWindows {
        windows,
        window_reads: Arc::clone(&window_reads),
        stop: Arc::clone(&stop),
        stops_at: None,
    });

    let outcome = SearchReadJob::Step {
        from_byte: file_size,
        toward: WalkToward::Older,
        file_size,
        query: "needle".into(),
    }
    .run(&source, &stop)
    .expect("the read was not stopped");

    assert!(
        matches!(outcome, SearchReadOutcome::NoStep),
        "the bound stopped the walk short of the window holding the match"
    );
    assert_eq!(
        window_reads.load(Ordering::Relaxed),
        SEARCH_WALK_MAX_WINDOWS,
        "the walk read exactly the windows its bound allows"
    );
}

/// A window that cannot be read comes back as a failure, not as a log that
/// ends here: the view keeps its rows, and a later step may well succeed.
#[test]
fn test_read_failure_is_reported() {
    let source: Arc<dyn TranscriptSnapshot> = Arc::new(UnreadableOlder);

    let opened = run(SearchReadJob::Open, &source).expect("the read was not stopped");
    assert!(
        matches!(opened, SearchReadOutcome::ReadFailed),
        "an unreadable tail window is a failure, not an empty log"
    );

    let stepped = run(
        SearchReadJob::Step {
            from_byte: 4096,
            toward: WalkToward::Older,
            file_size: 4096,
            query: "needle".into(),
        },
        &source,
    )
    .expect("the read was not stopped");
    assert!(
        matches!(stepped, SearchReadOutcome::ReadFailed),
        "an unreadable older window is a failure too"
    );
}

/// A want waits for the budget rather than starting beside the read already
/// running, and a wanted read that is replaced keeps the reader's latest ask.
#[test]
fn test_want_waits_for_budget() {
    let mut reads = SearchReads::default();
    let (_tx, rx) = channel::<SearchReadOutcome>();
    drop(reads.dispatch(SearchReadJob::Open, rx));

    reads.want(SearchWant::Step(WalkToward::Older));
    assert!(
        reads.take_want().is_none(),
        "a read is on file, so the want waits instead of starting a second walk"
    );

    drop(reads.take());
    assert_eq!(
        reads.take_want(),
        Some(SearchWant::Step(WalkToward::Older)),
        "the want is answered once the read it waited for left the file"
    );
}

/// A view that closes drops the read it asked for: the record goes, so the
/// outcome has nowhere to land, and the worker's flag is raised, so a walk
/// still inside the log stops at its next window.
#[test]
fn test_closed_view_drops_read() {
    let mut reads = SearchReads::default();
    let (_tx, rx) = channel::<SearchReadOutcome>();
    let lease = reads.dispatch(SearchReadJob::Open, rx);
    let stop = Arc::clone(lease.stop());
    reads.want(SearchWant::Step(WalkToward::Newer));
    let epoch = reads.epoch();

    reads.forget_closed_view();

    assert!(
        reads.take().is_none(),
        "the read on file went with the view"
    );
    assert_ne!(reads.epoch(), epoch, "the generation moved on");
    assert!(
        stop.load(Ordering::Relaxed),
        "the read in flight was told to stop rather than left to finish"
    );

    drop(lease);
    reads.want(SearchWant::Step(WalkToward::Newer));
    assert_eq!(
        reads.take_want(),
        Some(SearchWant::Step(WalkToward::Newer)),
        "the budget is free again, and the earlier want is not what is answered"
    );

    let settled = reads.epoch();
    reads.forget_closed_view();
    assert_eq!(
        reads.epoch(),
        settled,
        "a closed view with nothing to drop leaves the generation alone"
    );
}
