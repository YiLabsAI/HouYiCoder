//! Real-binary PTY journeys over the transcript's history read path. The unit
//! layer asserts the scroll anchor and the parked view state against App state;
//! this layer drives the real houyi binary through a real terminal so the rows
//! the user actually sees decide the outcome, and it changes the window height
//! while the session runs, which no other suite does.

use crate::common::{
    COLS, Key, PtySession, RENDER_TIMEOUT, fresh_temp_dir, launch_and_settle, parse_scroll_status,
    sample_rss_kb, seed_large_session_on_disk, top_row, view_line, view_total,
};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// The fixture size for the parked-view journey. Small enough that the whole
/// log stays inside the resident window, so the journey measures the parked
/// row across a window change rather than the read path behind it; the read
/// path has its own workload.
const JOURNEY_FIXTURE_BYTES: u64 = 4 * 1024 * 1024;

/// How far above the tail the journey parks. Each press moves about a
/// viewport, so this is a few screens back: past the settle marker, still
/// inside the loaded rows.
const PARK_PRESSES: usize = 6;

/// How long a press has to show its effect. Long enough that a scheduler
/// shared with two other suites cannot make a landed press look like a lost
/// one, short enough that a view that will not move is reported rather than
/// waited out.
const PRESS_WINDOW: Duration = Duration::from_secs(3);

/// Whether a captured frame was drawn at the given window height. Rows run to
/// the bottom of the window and the status row sits on the last of them, so a
/// frame captured while the window is still at its previous height fails both
/// counts: a shorter frame stops short of the rows below it, and a taller one
/// carries content where the status row belongs.
///
/// The row count is the number of text lines the capture carries, which equals
/// the window height only while every row ends in a line break at the terminal
/// level. The app positions each display row itself, so a row never wraps into
/// its neighbour and the two counts agree.
fn drawn_at_height(contents: &str, rows: u16) -> bool {
    let frame: Vec<&str> = contents.lines().collect();
    frame.len() == rows as usize
        && frame
            .last()
            .is_some_and(|row| parse_scroll_status(row).is_some())
}

/// A frame captured before the window grew stops short of the new height: the
/// capture drops the blank rows below a frame's last painted row, so the rows
/// it does carry are the frame drawn at the height before. The journey waits
/// for a redraw, and accepting that frame would report a redraw that has not
/// happened.
#[test]
fn test_stale_frame_rejected() {
    let stale = "body 1\nbody 2\n SCROLL  line 12/40  Esc=tail";
    assert!(!drawn_at_height(stale, 30));
    assert!(drawn_at_height(stale, 3));
}

/// A frame drawn at the new height reaches the window's bottom row, where the
/// layout puts the status row. Rows that fall short of it, or a bottom row
/// without the status, are frames of another height.
#[test]
fn test_fresh_frame_accepted() {
    let fresh = "body 1\nbody 2\nbody 3\n SCROLL  line 12/40  Esc=tail";
    assert!(drawn_at_height(fresh, 4));
    let no_status = "body 1\nbody 2\nbody 3\nbody 4";
    assert!(
        !drawn_at_height(no_status, 4),
        "the bottom row of a frame drawn here carries the status row"
    );
}

/// One page-up press together with the frame it settles on, or None when the
/// view did not move within the window.
///
/// The top row moving is the press landing; the frame is returned only once a
/// second reading agrees with it, because the terminal carries the rows of the
/// frame being painted and a reading taken mid-paint pairs a new top row with
/// the status row of the frame before it. The caller parks on the returned
/// frame, so the row it stands on and the line number beside it belong to one
/// frame.
fn press_page_up(session: &mut PtySession) -> Option<String> {
    let before = top_row(&session.screen().contents()).to_string();
    session.send_key(&Key::PageUp);
    let deadline = Instant::now() + PRESS_WINDOW;
    let mut settled: Option<String> = None;
    while Instant::now() < deadline {
        let now = session.screen().contents();
        let landed = top_row(&now) != before && view_line(&now).is_some();
        if landed && settled.as_deref() == Some(now.as_str()) {
            return Some(now);
        }
        settled = landed.then_some(now);
        sleep(Duration::from_millis(2));
    }
    None
}

/// The parked viewport holds the row it stands on when the window changes
/// height. The journey parks a few screens above the tail, changes the height
/// up and back down, and reads the same row at the top of the view with the
/// same line number in the scroll status. The width is held fixed so a move
/// cannot come from the row rewrapping, and the height returns to its starting
/// value at the end so a second run of the same size compares against the same
/// geometry.
#[test]
#[ignore = "PTY journey; the real binary replays a seeded log"]
fn test_park_survives_resize() {
    let dir = fresh_temp_dir("journey-resize");
    let sid = "77777777-7777-7777-7777-777777777777";
    let fixture = seed_large_session_on_disk(&dir, sid, "measure-model", JOURNEY_FIXTURE_BYTES);
    let (mut session, _) = launch_and_settle(&dir, &fixture);
    let pid = session.child_pid().expect("child pid");
    let rss_before = sample_rss_kb(pid);

    let mut parked: Option<String> = None;
    let mut pressed = 0;
    for _ in 0..PARK_PRESSES {
        if let Some(frame) = press_page_up(&mut session) {
            pressed += 1;
            parked = Some(frame);
        }
    }
    assert!(
        pressed > 1,
        "the journey parks above the tail: {pressed} of {PARK_PRESSES} presses moved the view, \
         so the view did not leave the rows it started on:\n{}",
        session.output()
    );
    let parked = parked.expect("a press that moved the view left the frame it settled on");
    let parked_top = top_row(&parked).to_string();
    let parked_line = view_line(&parked).expect("the status row answers once parked");
    let parked_total = view_total(&parked).expect("and carries the row total");
    assert!(
        parked_line > 1,
        "the journey leaves the oldest row behind: the view stands on line {parked_line}"
    );

    for rows in [30u16, 24u16] {
        session.resize(rows, COLS);
        let deadline = Instant::now() + RENDER_TIMEOUT;
        let mut held = false;
        let mut last = String::new();
        while Instant::now() < deadline {
            last = session.screen().contents();
            // A reading counts only once the frame is drawn at the new height
            // and reaches the window's bottom row, so a frame still at the old
            // height cannot stand in for the redraw.
            if drawn_at_height(&last, rows)
                && top_row(&last) == parked_top
                && view_line(&last) == Some(parked_line)
                && view_total(&last) == Some(parked_total)
            {
                held = true;
                break;
            }
            sleep(Duration::from_millis(2));
        }
        assert!(
            held,
            "the parked row holds the top line at {rows} rows; the status row reads {:?} with \
             top row {:?}:\n{last}",
            parse_scroll_status(&last),
            top_row(&last)
        );
    }

    let rss_after = sample_rss_kb(pid);
    println!(
        "parked view resize: top row held at line {parked_line}/{parked_total}, rss {rss_before:?} -> {rss_after:?} KiB"
    );
    // The journey's own work is what this bounds: parking, two height changes,
    // and the redraws they cause. Growth against the log's size is measured by
    // its own workload, not here.
    if let (Some(before), Some(after)) = (rss_before, rss_after) {
        assert!(
            after < before + 32 * 1024,
            "the window changes add no more than 32 MiB: {before} -> {after} KiB"
        );
    }
}
