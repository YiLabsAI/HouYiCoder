//! A viewport parked on a row the rows on view left out: the row comes back
//! from the session log when the log still holds it, and the reader is told
//! when it does not. The place is never given up silently, and no read is
//! spent on a row no read could bring back.

use super::*;

use crate::transcript::SequencedFrame;
use crate::transcript::row_id::RowId;
use houyicoder_protocol::envelope::DurableEventId;

/// A frame the log named, so its row carries an identity a read can look for.
/// One wrapper names one entry, and one entry yields one row.
fn durable_frame(id: &str, frame: TranscriptFrame) -> SequencedFrame {
    SequencedFrame {
        seq: None,
        event_id: Some(DurableEventId(id.to_string())),
        frame,
    }
}

/// A view the byte budget drained past its start, so the rows on view belong
/// to the frames at the resident front and the next drain reclaims them. The
/// frames carry log identities, which is what a read looks rows up by, or
/// none, which is what a view with no log to read has.
fn drained_view(named: bool) -> App {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.transcript_mut().set_resident_byte_budget(4096);
    let text = "x".repeat(120);
    for i in 0..60 {
        let frame = agent_msg(&format!("row {i} {text}"));
        if named {
            app.transcript_mut()
                .push_frame(durable_frame(&format!("e{i}"), frame));
        } else {
            app.transcript_mut().push_frame(frame);
        }
    }
    app.rebuild_transcript();
    app.rebuild_transcript();
    assert!(
        app.transcript().frame_window_start() > 0,
        "the budget drained the oldest frames"
    );
    app
}

/// Take the frames that leave the view under the parked row out of the log's
/// window: a heavy tail the budget drains past the row the reader stands on.
/// The pass that drains still draws the rows it reclaimed, so the view settles
/// over the resident window alone before the row is gone from it.
fn drain_past_parked_row(app: &mut App) {
    let text = "y".repeat(200);
    for i in 0..40 {
        app.transcript_mut().push_frame(durable_frame(
            &format!("late{i}"),
            agent_msg(&format!("later {i} {text}")),
        ));
    }
    app.rebuild_transcript();
    app.rebuild_transcript();
}

/// The rows on view beside the identity of each, the form a log window carries
/// and the form a read matches a window against the view by.
fn view_rows(app: &App) -> Vec<(RowId, TranscriptLine)> {
    let lines: Vec<TranscriptLine> = app.transcript().iter().cloned().collect();
    let ids: Vec<RowId> = app.transcript().row_ids().to_vec();
    assert_eq!(
        lines.len(),
        ids.len(),
        "every row on view carries its identity"
    );
    ids.into_iter().zip(lines).collect()
}

/// One window of the session log holding these rows under these identities.
fn named_window(rows: &[(RowId, TranscriptLine)], start: u64, next: u64) -> WindowLoad {
    WindowLoad {
        rows: NamedRows::from_parts(
            rows.iter().map(|(_, line)| line.clone()).collect(),
            rows.iter().map(|(id, _)| id.clone()).collect(),
        ),
        start_offset: start,
        next_offset: next,
        skipped: 0,
        bytes_total: next,
    }
}

/// A log source returning these windows, newest last, over a log of this size.
fn named_log_source(windows: Vec<WindowLoad>, log_bytes: u64) -> Arc<MockSnapshot> {
    Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows,
        scan_steps: 0,
        scan_calls: AtomicU32::new(0),
    })
}

/// The screen row whose drawn text holds this marker, which is how a test
/// names the row a reader parks on without counting display rows itself.
fn screen_row_holding(app: &App, marker: &str) -> usize {
    (0..20)
        .find(|row| transcript_row(app, *row).contains(marker))
        .unwrap_or_else(|| panic!("the pane draws a row holding {marker:?}"))
}

/// The identity the viewport stands on, which is the row a read is sent for.
fn parked_row(app: &App) -> RowId {
    app.whole_scroll()
        .anchor()
        .expect("the viewport is parked")
        .row
        .clone()
}

/// The row the reader stands on leaves the view in a drain, and the log still
/// holds it: the pane stands on that row again, no notice tells the reader the
/// place is gone, and the rows the walk crossed came back under it.
#[test]
fn test_parked_row_comes_back() {
    let mut app = drained_view(true);
    let log_rows = view_rows(&app);

    draw_transcript(&mut app);
    app.park_scroll_at(0);
    let parked = transcript_row(&app, 0);
    let chased = parked_row(&app);
    assert!(
        chased.is_durable(),
        "the park stands on a row the log named: {chased:?}"
    );

    drain_past_parked_row(&mut app);
    assert!(
        !view_rows(&app).iter().any(|(id, _)| *id == chased),
        "the drain took the parked row out of the view"
    );

    // The log still holds the rows the drain took, up to the rows the pane
    // shows now: the walk joins against those and keeps everything above them.
    let mut window = log_rows.clone();
    window.extend(view_rows(&app));
    app.sessions.persistence.snapshot = Some(named_log_source(
        vec![named_window(&window, 100, 4096)],
        4096,
    ));
    app.pump_background_reads();
    assert!(
        pump_history_read_blocking(&mut app, Duration::from_secs(5)),
        "the read sent for the parked row landed"
    );

    assert!(
        app.transcript().disk_row_count() > 0,
        "the rows the walk crossed came back from the log: front={:?}",
        app.transcript().disk_front()
    );
    assert!(
        [transcript_row(&app, 0), transcript_row(&app, 1)].contains(&parked),
        "the pane stands on the parked row again: {:?}",
        (transcript_row(&app, 0), transcript_row(&app, 1))
    );
    assert!(
        !render_text(&app, 100, 30).contains("scroll position released"),
        "nothing took the place away"
    );
}

/// A parked row the view read back from the log leaves with the loaded rows
/// when a drain clears them, and the log still holds it: the walk reads the
/// rows older than the pane's front, finds it among them, and the pane stands
/// on it again.
#[test]
fn test_parked_loaded_row_returns() {
    let mut app = drained_view(true);
    let loaded: Vec<(RowId, TranscriptLine)> = (0..2)
        .map(|i| {
            (
                RowId::from(format!("log{i}").as_str()),
                TranscriptLine::Agent(format!("logged {i}")),
            )
        })
        .collect();
    let mut window = loaded.clone();
    window.extend(view_rows(&app));
    app.sessions.persistence.snapshot = Some(named_log_source(
        vec![named_window(&window, 100, 4096)],
        4096,
    ));

    // Step back to the top of the view and read the log rows in above it.
    draw_transcript(&mut app);
    for _ in 0..4 {
        app.scroll_transcript_up();
    }
    app.load_older_frames();
    assert!(
        pump_history_read_blocking(&mut app, Duration::from_secs(5)),
        "the log rows landed"
    );
    assert_eq!(
        app.transcript().disk_row_count(),
        2,
        "the log rows are on view"
    );

    // Step up onto the oldest loaded row: the pane's top is a row only the log
    // holds, so a drain that clears the loaded rows takes the parked row with
    // them.
    app.scroll_transcript_up();
    let parked = transcript_row(&app, 0);
    let chased = parked_row(&app);
    assert_eq!(chased, loaded[0].0, "the park stands on a loaded row");

    drain_past_parked_row(&mut app);
    assert_eq!(
        app.transcript().disk_row_count(),
        0,
        "the drain cleared the loaded rows"
    );

    // The log still holds the parked row, and the window carries the rows the
    // pane shows now, so the walk joins above them and reaches it.
    let mut window = loaded.clone();
    window.extend(view_rows(&app));
    app.sessions.persistence.snapshot = Some(named_log_source(
        vec![named_window(&window, 100, 4096)],
        4096,
    ));
    app.pump_background_reads();
    assert!(
        pump_history_read_blocking(&mut app, Duration::from_secs(5)),
        "the read sent for the loaded row landed"
    );

    assert!(
        [transcript_row(&app, 0), transcript_row(&app, 1)].contains(&parked),
        "the pane stands on the loaded row again: {:?}",
        (transcript_row(&app, 0), transcript_row(&app, 1))
    );
    assert!(
        !render_text(&app, 100, 30).contains("scroll position released"),
        "nothing took the place away"
    );
}

/// A row the frontend raised has no entry in the log to read back, so a
/// viewport parked on one gives the place up with the reader told, and no read
/// is spent looking for a name no entry carries. A row still on view keeps its
/// place: the release answers a row the pane stopped carrying, not a park.
#[test]
fn test_local_parked_row_releases() {
    let mut app = drained_view(false);
    app.sessions.persistence.snapshot =
        Some(named_log_source(vec![named_window(&[], 100, 4096)], 4096));

    draw_transcript(&mut app);
    app.park_scroll_at(0);
    let parked = transcript_row(&app, 0);
    let chased = parked_row(&app);
    assert!(
        !chased.is_durable(),
        "a view with no log to read names its rows by position: {chased:?}"
    );

    app.pump_background_reads();
    assert!(
        !app.whole_scroll().is_following_tail(),
        "a row the pane still draws keeps its place"
    );

    drain_past_parked_row(&mut app);
    app.pump_background_reads();

    assert!(
        app.whole_scroll().is_following_tail(),
        "the viewport returned to the tail"
    );
    let drawn = render_text(&app, 100, 30);
    assert!(
        drawn.contains("left the history"),
        "the reader is told the row is gone, and the parked row was {parked:?}"
    );
    assert!(
        !drawn.contains("further back than the view reads"),
        "no read was sent for a name the log cannot carry"
    );
    assert!(
        !app.sessions.current.history_reads.holds_pending(),
        "nothing read the log for a row the frontend raised"
    );
}

/// A parked row with no log to read it from cannot be brought back, so the
/// viewport gives up the place and says why rather than holding a screen row
/// whose content never returns.
#[test]
fn test_unreachable_parked_row_releases() {
    let mut app = drained_view(true);
    assert!(
        app.sessions.persistence.snapshot.is_none(),
        "no log is wired for this view"
    );

    draw_transcript(&mut app);
    app.park_scroll_at(0);
    let chased = parked_row(&app);
    assert!(chased.is_durable(), "the park stands on a named row");

    drain_past_parked_row(&mut app);
    app.pump_background_reads();

    assert!(
        app.whole_scroll().is_following_tail(),
        "the viewport returned to the tail"
    );
    assert!(
        render_text(&app, 100, 30).contains("further back than the view reads"),
        "the reader is told the place cannot be held"
    );
    assert_eq!(
        app.transcript().disk_row_count(),
        0,
        "no rows were read for it"
    );
}

/// A walk that spends its byte budget with the log still holding older rows
/// does not decide the row is gone: the viewport is released with a verdict
/// that says the row is out of reach, and the front stays open so a later
/// scroll can read further back.
#[test]
fn test_parked_row_beyond_budget() {
    let mut app = drained_view(true);
    let parked = {
        draw_transcript(&mut app);
        app.park_scroll_at(0);
        parked_row(&app)
    };
    drain_past_parked_row(&mut app);

    // A log whose windows sit far apart: the walk crosses the window holding
    // the pane's rows, then a step older than its whole budget, so it stops
    // with the log's start still ahead of it.
    let older: Vec<(RowId, TranscriptLine)> = (0..2)
        .map(|i| {
            (
                RowId::from(format!("old{i}").as_str()),
                TranscriptLine::Agent(format!("older {i}")),
            )
        })
        .collect();
    let far: Vec<(RowId, TranscriptLine)> = vec![(
        RowId::from("farther"),
        TranscriptLine::Agent("farther back".into()),
    )];
    let mut newest = older.clone();
    newest.extend(view_rows(&app));
    let megabyte = 1024 * 1024;
    app.sessions.persistence.snapshot = Some(named_log_source(
        vec![
            named_window(&far, 80 * megabyte, 90 * megabyte),
            named_window(&newest, 90 * megabyte, 100 * megabyte),
        ],
        100 * megabyte,
    ));
    app.pump_background_reads();
    assert!(
        pump_history_read_blocking(&mut app, Duration::from_secs(5)),
        "the walk answered"
    );

    assert!(
        app.whole_scroll().is_following_tail(),
        "the viewport returned to the tail"
    );
    assert!(
        render_text(&app, 100, 30).contains("further back than the view reads"),
        "a walk that ran out of bytes reports the row out of reach, for {parked:?}"
    );
    assert_eq!(
        app.transcript().disk_row_count(),
        0,
        "a walk that reached nothing adds no rows"
    );
    assert_ne!(
        app.transcript().disk_front(),
        DiskFront::Stopped,
        "the front stays open, since the log holds older rows"
    );
}

/// A view holding two turns, the second of which a rewind takes back.
fn two_turn_view() -> App {
    let mut app = fresh_app();
    app.ui.navigation.screen = Screen::Working;
    app.transcript_mut()
        .push_frame(durable_frame("t0", user_msg("first turn")));
    for i in 0..4 {
        app.transcript_mut().push_frame(durable_frame(
            &format!("a{i}"),
            agent_msg(&format!("answer {i}")),
        ));
    }
    app.transcript_mut()
        .push_frame(durable_frame("t1", user_msg("second turn")));
    for i in 0..4 {
        app.transcript_mut().push_frame(durable_frame(
            &format!("b{i}"),
            agent_msg(&format!("reply {i}")),
        ));
    }
    app.rebuild_transcript();
    app
}

/// A rewind truncates the frames under a parked row, which is a tear of the
/// history rather than a scroll further back: the row cannot be read back from
/// below, so the decision is made where the rows are torn off. The viewport
/// returns to the tail, the reader is told the row left the history, and no
/// read is spent walking the log for a row that sits newer than the pane.
#[test]
fn test_rewind_releases_parked_row() {
    let mut app = two_turn_view();
    app.sessions.persistence.snapshot =
        Some(named_log_source(vec![named_window(&[], 100, 4096)], 4096));

    draw_transcript(&mut app);
    app.park_scroll_at(screen_row_holding(&app, "reply 3"));
    let chased = parked_row(&app);
    assert!(chased.is_durable(), "the park stands on a named row");

    app.rewind_to_last_user_input();

    assert!(
        view_rows(&app)
            .iter()
            .all(|(_, line)| !line.render().contains("reply 3")),
        "the torn rows left the view"
    );
    assert!(
        app.whole_scroll().is_following_tail(),
        "the viewport returned to the tail"
    );
    let drawn = render_text(&app, 100, 30);
    assert!(
        drawn.contains("left the history"),
        "the reader is told the row was torn off, for {chased:?}"
    );
    assert!(
        !drawn.contains("further back than the view reads"),
        "the tear-off site decided the row's fate, not a read"
    );
    assert!(
        !app.sessions.current.history_reads.holds_pending(),
        "no read was dispatched for a row the rewind removed"
    );
}

/// Send the read for the row the viewport stands on while the log holds
/// neither it nor a window reaching further back: the walk joins the rows on
/// view and then crosses to the log's start without the row. A verdict about
/// the parked row is on its way, which is what a reader who moves before it
/// lands must not be given.
fn chase_missing_row(app: &mut App) {
    let window = view_rows(app);
    app.sessions.persistence.snapshot = Some(named_log_source(
        vec![named_window(&window, 100, 4096)],
        4096,
    ));
    app.pump_background_reads();
    assert!(
        app.sessions.current.history_reads.holds_pending(),
        "a read for the parked row is on its way"
    );
}

/// A reader who moved to another row while a read for an older one ran: the
/// verdict answers the row the read was sent for, not whatever row the reader
/// stands on when it lands, so the place they moved to is left alone.
#[test]
fn test_verdict_spares_reader() {
    let mut app = drained_view(true);
    draw_transcript(&mut app);
    app.park_scroll_at(0);
    let chased = parked_row(&app);
    drain_past_parked_row(&mut app);
    assert!(chased.is_durable(), "the parked row is one the log named");

    chase_missing_row(&mut app);

    draw_transcript(&mut app);
    app.park_scroll_at(1);
    let moved = parked_row(&app);
    assert_ne!(moved, chased, "the reader parked on a different row");

    assert!(
        pump_history_read_blocking(&mut app, Duration::from_secs(5)),
        "the read answered"
    );
    assert_eq!(
        parked_row(&app),
        moved,
        "the verdict about the older row did not move the viewport"
    );
    assert!(
        !app.whole_scroll().is_following_tail(),
        "the place the reader moved to stands"
    );
    assert!(
        !render_text(&app, 100, 30).contains("scroll position released"),
        "a verdict for a row the reader left releases nothing"
    );
}

/// A reader who returned to the tail while a read for an older row ran: the
/// verdict answers a place nobody holds any more, so the viewport stays at the
/// tail and the log is left as it was found, with a later scroll back still
/// reading rows older than the ones on view.
#[test]
fn test_verdict_spares_log() {
    let mut app = drained_view(true);
    draw_transcript(&mut app);
    app.park_scroll_at(0);
    let chased = parked_row(&app);
    drain_past_parked_row(&mut app);
    assert!(chased.is_durable(), "the parked row is one the log named");

    chase_missing_row(&mut app);
    app.scroll_transcript_follow_tail();
    assert!(
        app.whole_scroll().is_following_tail(),
        "the reader returned to the tail"
    );

    assert!(
        pump_history_read_blocking(&mut app, Duration::from_secs(5)),
        "the read answered"
    );
    assert!(
        app.whole_scroll().is_following_tail(),
        "the viewport stays at the tail"
    );
    assert!(
        !render_text(&app, 100, 30).contains("scroll position released"),
        "no place was given up"
    );

    // The verdict answered a row nobody was parked on, so it must not have
    // closed the log: a scroll back still reads rows older than the ones on
    // view.
    let mut window = vec![(
        RowId::from("older0"),
        TranscriptLine::Agent("logged older".into()),
    )];
    window.extend(view_rows(&app));
    app.sessions.persistence.snapshot = Some(named_log_source(
        vec![named_window(&window, 100, 4096)],
        4096,
    ));
    for _ in 0..4 {
        app.scroll_transcript_up();
    }
    app.load_older_frames();
    assert!(
        pump_history_read_blocking(&mut app, Duration::from_secs(5)),
        "the scroll back sent a read"
    );
    // Bring the returned rows into view and read the screen: the older row
    // landed above the place the reader holds, so the visible proof is the
    // row's own text once the viewport reaches the top.
    for _ in 0..40 {
        app.scroll_transcript_up();
    }
    draw_transcript(&mut app);
    assert!(
        render_text(&app, 100, 30).contains("logged older"),
        "the older rows came back, so the log stays open"
    );
}

/// A rewind takes the newest turn off the tail, and a viewport parked in an
/// earlier turn keeps its place: the tear is below the row the reader stands
/// on, so nothing about that row changed and the place is not given up.
#[test]
fn test_rewind_spares_parked_row() {
    let mut app = two_turn_view();

    draw_transcript(&mut app);
    app.park_scroll_at(screen_row_holding(&app, "answer 2"));
    let parked = transcript_row(&app, 0);
    let chased = parked_row(&app);
    assert!(chased.is_durable(), "the park stands on a named row");

    app.rewind_to_last_user_input();

    assert!(
        !render_text(&app, 100, 30).contains("scroll position released"),
        "the place stands, since the torn rows were below it"
    );
    assert_eq!(
        transcript_row(&app, 0),
        parked,
        "the pane still starts where the reader put it"
    );
}
