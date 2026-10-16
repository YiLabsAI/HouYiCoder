use super::*;
use crate::fold::compute_fold_groups;
use crate::scroll::VIEWABLE_SCROLLBACK_CAP;
use crate::transcript::NamedRows;

/// The rows a scroll back to the resident front reads from the session log:
/// the frames there were drained, so the log is the only source for them. The
/// tail read carries the rows the view already shows, and the overlap drops them.
#[test]
fn test_scrollback_reads_log_rows() {
    let mut app = drained_app();
    let front_frame = app.transcript().frame_window_start();
    let mut lines: Vec<TranscriptLine> = (0..5)
        .map(|i| TranscriptLine::User(format!("older {i}")))
        .collect();
    lines.extend(app.transcript().iter().cloned());
    app.sessions.persistence.snapshot = Some(log_source(vec![log_window(lines, 100, 4096)]));

    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));

    assert_eq!(
        app.transcript().disk_row_count(),
        5,
        "the log's older rows landed"
    );
    let head: Vec<String> = app
        .transcript()
        .iter()
        .take(5)
        .map(|l| l.render())
        .collect();
    let want: Vec<String> = (0..5)
        .map(|i| TranscriptLine::User(format!("older {i}")).render())
        .collect();
    assert_eq!(head, want, "the oldest loaded row leads the view");
    assert!(
        matches!(app.transcript().get(5), Some(TranscriptLine::User(t)) if t.starts_with(&format!("{front_frame} "))),
        "the resident front follows the loaded rows: {:?}",
        app.transcript().get(5)
    );
    assert_rows_print_once(&app);
    assert_eq!(app.transcript().disk_front(), DiskFront::At(100));
    assert_eq!(
        app.whole_scroll().raw_top(),
        5,
        "the viewport held still under the rows that landed above it"
    );
}

/// Older rows keep arriving as the reader scrolls past what is loaded: each
/// read ends where the loaded rows begin, and the last one reaching the log's
/// start ends the scroll back rather than reading the log again.
#[test]
fn test_scrollback_chains_older_window() {
    let mut app = drained_app();
    let mut tail: Vec<TranscriptLine> = (0..5)
        .map(|i| TranscriptLine::User(format!("older {i}")))
        .collect();
    tail.extend(app.transcript().iter().cloned());
    let oldest = vec![
        TranscriptLine::User("oldest 0".into()),
        TranscriptLine::User("oldest 1".into()),
    ];
    app.sessions.persistence.snapshot = Some(log_source(vec![
        log_window(oldest, 0, 100),
        log_window(tail, 100, 4096),
    ]));

    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));

    assert_eq!(app.transcript().disk_row_count(), 7, "both windows landed");
    assert!(
        matches!(app.transcript().first(), Some(TranscriptLine::User(t)) if t == "oldest 0"),
        "the second read landed above the first"
    );
    assert_eq!(app.transcript().disk_front(), DiskFront::At(0));
    assert_rows_print_once(&app);

    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));
    assert_eq!(
        app.transcript().disk_row_count(),
        7,
        "a read at the log's start adds nothing"
    );
    assert_eq!(
        app.transcript().disk_front(),
        DiskFront::Stopped,
        "the scroll back stops at the log's start"
    );
}

/// Without a wired session log there is nothing to read, so the scroll back
/// stops at the resident front as it did before the log was reachable.
#[test]
fn test_scrollback_without_log_source() {
    let mut app = drained_app();
    let shown = app.transcript().len();
    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));
    assert_eq!(app.transcript().len(), shown);
    assert_eq!(app.transcript().disk_front(), DiskFront::Unloaded);
}

/// A window holding no row the visible front matches is not shown: the rows
/// cannot be placed against the view, and a guessed overlap would print content
/// twice or lose it. The read is remembered as stopping, so standing at the
/// front does not read the log again on every pass.
#[test]
fn test_scrollback_stops_without_match() {
    let mut app = drained_app();
    let shown = app.transcript().len();
    app.sessions.persistence.snapshot = Some(log_source(vec![log_window(
        vec![TranscriptLine::Agent("unrelated row".into())],
        100,
        4096,
    )]));

    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));

    assert_eq!(app.transcript().disk_row_count(), 0);
    assert_eq!(app.transcript().len(), shown, "the view is untouched");
    assert_eq!(app.transcript().disk_front(), DiskFront::Stopped);
}

/// Rows read back from the session log survive the rebuilds that follow, and
/// they leave with the view when the reader returns to the tail: the capped
/// tail view cannot reach them, so holding them would only cost memory.
#[test]
fn test_rows_leave_at_end() {
    let mut app = drained_app();
    let mut lines: Vec<TranscriptLine> = vec![TranscriptLine::User("older 0".into())];
    lines.extend(app.transcript().iter().cloned());
    app.sessions.persistence.snapshot = Some(log_source(vec![log_window(lines, 100, 4096)]));

    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));
    assert_eq!(app.transcript().disk_row_count(), 1);

    app.transcript_mut().push_frame(agent_msg("answer"));
    app.rebuild_transcript();
    assert_eq!(
        app.transcript().disk_row_count(),
        1,
        "the loaded row survives the rebuild"
    );
    assert!(
        app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t == "older 0"))
    );

    app.whole_scroll_mut().follow_tail();
    app.rebuild_transcript();
    assert_eq!(
        app.transcript().disk_row_count(),
        0,
        "the loaded row leaves with the view it was read for"
    );
    assert_eq!(app.transcript().disk_front(), DiskFront::Unloaded);
    assert!(
        !app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(t) if t == "older 0"))
    );
    assert!(
        app.transcript()
            .iter()
            .any(|l| matches!(l, TranscriptLine::Agent(t) if t == "answer")),
        "the tail row stays"
    );
}

/// A row text the log repeats must not cut the window at the wrong place: the
/// window holds an older copy of the first rows the view shows, and the overlap
/// has to land where the match runs on rather than where the text first
/// appears.
#[test]
fn test_join_prefers_longest_run() {
    let mut app = drained_app();
    let view: Vec<TranscriptLine> = app.transcript().iter().cloned().collect();
    let mut lines: Vec<TranscriptLine> = vec![
        view[0].clone(),
        view[1].clone(),
        view[2].clone(),
        TranscriptLine::User("unrelated".into()),
    ];
    lines.extend(view.clone());
    app.sessions.persistence.snapshot = Some(log_source(vec![log_window(lines, 100, 4096)]));

    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));

    assert_eq!(
        app.transcript().disk_row_count(),
        4,
        "the overlap landed past the short match"
    );
    assert_eq!(
        app.transcript().get(3).map(|l| l.render()),
        Some(TranscriptLine::User("unrelated".into()).render()),
        "the loaded rows end above the view's own first row"
    );
}

/// The row the view starts at can sit further back than one window of the log:
/// the walk crosses into the older window, and the rows that continue the
/// match live in the newer one.
#[test]
fn test_walk_crosses_log_windows() {
    let mut app = drained_app();
    let view: Vec<TranscriptLine> = app.transcript().iter().cloned().collect();
    let mut older: Vec<TranscriptLine> = vec![
        TranscriptLine::User("old 0".into()),
        TranscriptLine::User("old 1".into()),
        view[0].clone(),
    ];
    older.extend(view[1..].iter().cloned());
    let newer: Vec<TranscriptLine> = view[1..].to_vec();
    app.sessions.persistence.snapshot = Some(log_source(vec![
        log_window(older, 0, 100),
        log_window(newer, 100, 4096),
    ]));

    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));

    assert_eq!(
        app.transcript().disk_row_count(),
        2,
        "the walk crossed into the older window and kept the rows above the overlap"
    );
    assert!(
        matches!(app.transcript().first(), Some(TranscriptLine::User(t)) if t == "old 0"),
        "the oldest loaded row leads the view"
    );
    assert_eq!(app.transcript().disk_front(), DiskFront::At(0));
}

/// A window that does not begin below where the walk asked would leave the
/// walk standing still, reading the same rows for ever. The port the TUI
/// owns may not return one, so the walk has to end on its own.
#[test]
fn test_walk_stops_without_progress() {
    let mut app = drained_app();
    let lines: Vec<TranscriptLine> = (0..8)
        .map(|i| TranscriptLine::User(format!("unrelated {i}")))
        .collect();
    app.sessions.persistence.snapshot = Some(log_source(vec![log_window(lines, 4096, 4096)]));

    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));

    assert_eq!(app.transcript().disk_row_count(), 0, "no rows were read");
    assert_eq!(app.transcript().disk_front(), DiskFront::Stopped);
}

/// The walk stops when the source holds no window for the offset it asks from,
/// so a log whose older window is missing ends the read rather than repeating
/// rows. The mismatch case — a window returned that does not end at the anchor
/// — is refused by the read itself, which the read layer's own test covers.
#[test]
fn test_walk_stops_without_window() {
    let mut app = drained_app();
    let view: Vec<TranscriptLine> = app.transcript().iter().cloned().collect();
    let mut older: Vec<TranscriptLine> = vec![TranscriptLine::User("old 0".into())];
    older.extend(view);
    let older_rows = NamedRows::from_lines(older);
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 5000,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows: vec![WindowLoad {
            lines: older_rows.lines,
            row_ids: older_rows.ids,
            start_offset: 100,
            next_offset: 5005,
            skipped: 0,
            bytes_total: 5000,
        }],
        scan_steps: 0,
        scan_calls: AtomicU32::new(0),
    }));

    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));

    assert_eq!(
        app.transcript().disk_row_count(),
        0,
        "no window answers the offset, so no row is taken"
    );
    assert_eq!(app.transcript().disk_front(), DiskFront::Stopped);
}

/// The dispatch front is captured after the Unloaded branch's own rebuild, not
/// before it. That rebuild can drain frames and advance the resident front, so
/// a pre-rebuild capture would make the first read's own result look stale and
/// drop it the moment the pump compares it to the current front.
#[test]
fn test_dispatch_front_after_drain() {
    let mut app = drained_app();
    // Push payload-heavy frames without rebuilding so the dispatch's own
    // rebuild drains and advances the resident front.
    let text = "z".repeat(2000);
    for i in 0..40 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("drain {i} {text}")));
    }
    let front_before = app.transcript().frame_window_start();
    app.sessions.persistence.snapshot = Some(log_source(vec![log_window(
        vec![TranscriptLine::User("x".into())],
        100,
        4096,
    )]));
    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    let front_after = app.transcript().frame_window_start();
    assert!(
        front_after > front_before,
        "the in-dispatch rebuild drained the resident front"
    );
    let pending = app
        .sessions
        .current
        .history_reads
        .take()
        .expect("a read was dispatched");
    assert_eq!(
        pending.dispatch_front, front_after,
        "dispatch_front is the front after the rebuild, not before it"
    );
}

/// against. If it did, the reader could not re-dispatch at the new front the
/// user scrolled to. The pump compares the dispatch front to the current
/// frame_window_start, not to the disk rows' join frame, so a first read
/// whose join frame is still zero is not wrongly rejected either.
#[test]
fn test_stale_history_read_dropped() {
    let mut app = drained_app();
    // Window rows share no text with the view, so the walk finds no overlap
    // and the read returns Exhausted.
    let lines: Vec<TranscriptLine> = (0..8)
        .map(|i| TranscriptLine::User(format!("unrelated {i}")))
        .collect();
    app.sessions.persistence.snapshot = Some(log_source(vec![log_window(lines, 100, 4096)]));
    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    // Move the resident front past the dispatch front before the result lands.
    let text = "z".repeat(2000);
    for i in 0..40 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("drain {i} {text}")));
    }
    app.rebuild_transcript();
    assert!(
        app.transcript().frame_window_start() > 0,
        "the budget drained the oldest frames"
    );
    pump_history_read_blocking(&mut app, Duration::from_secs(5));
    assert_ne!(
        app.transcript().disk_front(),
        DiskFront::Stopped,
        "stale Exhausted did not latch Stopped at the old front"
    );
    wait_for_workers_to_leave(&app);
    assert_eq!(
        app.sessions.current.history_reads.running(),
        0,
        "throwing the stale result away still let its worker leave, so the \
         budget is free for the scroll that asks again"
    );
}

/// The loaded rows reach the view only while the resident front they sit above
/// stays put. A drain moves that front past them, and the frames between the
/// two fronts are gone, so the rows leave rather than leave a hole.
#[test]
fn test_rows_leave_on_drain() {
    let mut app = drained_app();
    // A folded call/result pair, so the release has a group to shift.
    pump(&mut app, tool_call("c9", "glob"));
    pump(&mut app, tool_result("c9"));
    app.rebuild_transcript();
    let front = app.transcript().frame_window_start();
    let mut lines: Vec<TranscriptLine> = vec![TranscriptLine::User("older 0".into())];
    lines.extend(app.transcript().iter().cloned());
    app.sessions.persistence.snapshot = Some(log_source(vec![log_window(lines, 100, 4096)]));

    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));
    assert_eq!(app.transcript().disk_row_count(), 1);
    assert_eq!(app.transcript().disk_rows_front(), front);
    assert_eq!(
        app.whole_scroll().raw_top(),
        1,
        "the loaded row carried the viewport down with it"
    );

    let text = "y".repeat(200);
    for i in 0..40 {
        app.transcript_mut()
            .push_frame(user_msg(&format!("later {i} {text}")));
    }
    app.rebuild_transcript();

    assert!(
        app.transcript().frame_window_start() > front,
        "the budget drained past the front the rows were read above"
    );
    assert_eq!(
        app.transcript().disk_row_count(),
        0,
        "the loaded row left with the front it sat above"
    );
    assert_eq!(app.transcript().disk_front(), DiskFront::Unloaded);
    assert_eq!(
        app.whole_scroll().raw_top(),
        0,
        "the viewport holds its place as the row leaves in the drain's own pass"
    );
    let cached: Vec<(usize, usize)> = app
        .transcript()
        .fold_groups()
        .iter()
        .map(|g| (g.start, g.end))
        .collect();
    let fresh: Vec<(usize, usize)> =
        compute_fold_groups(app.transcript().lines(), app.agent_busy())
            .iter()
            .map(|g| (g.start, g.end))
            .collect();
    assert_eq!(
        cached, fresh,
        "the fold cache indexes the list the release left"
    );
}

/// The rows read back from the log are bounded: once the view holds the row
/// budget's worth of them, standing at the front reads no further.
#[test]
fn test_log_rows_capped() {
    let mut app = fresh_app();
    let rows: Vec<TranscriptLine> = (0..VIEWABLE_SCROLLBACK_CAP)
        .map(|i| TranscriptLine::User(format!("older {i}")))
        .collect();
    app.transcript_mut()
        .prepend_disk_rows(NamedRows::from_lines(rows), 100, 0);
    app.whole_scroll_mut().jump_to(0);
    app.sessions.persistence.snapshot = Some(log_source(vec![log_window(
        vec![TranscriptLine::User("even older".into())],
        50,
        100,
    )]));

    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));

    assert_eq!(
        app.transcript().disk_row_count(),
        VIEWABLE_SCROLLBACK_CAP,
        "the loaded rows stay at the budget"
    );
    assert_eq!(
        app.transcript().disk_front(),
        DiskFront::At(100),
        "no further window was read"
    );
}

/// A view with no rows has no front to match against, so the read returns
/// without touching the log.
#[test]
fn test_scrollback_without_rows() {
    let mut app = fresh_app();
    app.sessions.persistence.snapshot = Some(log_source(vec![log_window(
        vec![TranscriptLine::User("older 0".into())],
        100,
        4096,
    )]));

    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));

    assert_eq!(app.transcript().disk_row_count(), 0);
    assert_eq!(app.transcript().disk_front(), DiskFront::Unloaded);
}

/// A read that fails does not latch the front closed the way a true end does:
/// the front stays where it was, the cooldown holds off the next dispatch,
/// and the failure surfaces as a notice instead of silence.
#[test]
fn test_failed_read_front_open() {
    let mut app = drained_app();
    let shown = app.transcript().len();
    app.sessions.persistence.snapshot = Some(failing_log_source());

    app.whole_scroll_mut().jump_to(0);
    app.load_older_frames();
    pump_history_read_blocking(&mut app, Duration::from_secs(5));

    assert_eq!(app.transcript().len(), shown, "the view is untouched");
    assert_eq!(
        app.transcript().disk_front(),
        DiskFront::Unloaded,
        "a failed read does not latch the front closed"
    );
    assert!(
        !app.sessions.current.history_reads.may_dispatch(),
        "the cooldown holds off the next dispatch"
    );
    assert!(
        app.ui
            .notifications
            .current()
            .is_some_and(|n| n.key == "history-read-failed"),
        "the failure surfaced as a notice"
    );
}
