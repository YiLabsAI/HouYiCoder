//! Real-binary PTY journeys over the transcript's history read path. The unit
//! layer asserts the scroll anchor and the parked view state against App state;
//! this layer drives the real houyi binary through a real terminal so the rows
//! the user actually sees decide the outcome, and it changes the window height
//! while the session runs, which no other suite does.

use crate::common::{
    COLS, Key, PtySession, RENDER_TIMEOUT, fresh_temp_dir, launch_and_settle,
    launch_and_settle_with, parse_scroll_status, sample_rss_kb, seed_large_session_on_disk,
    top_row, view_line, view_total,
};
use houyicoder_config::ENV_HOUYICODER_TEST_RESIDENT_BYTE_BUDGET;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// The fixture size both journeys read: several screens of rows, and far more
/// bytes than the eviction journey lets the window keep.
const JOURNEY_FIXTURE_BYTES: u64 = 4 * 1024 * 1024;

/// The resident bytes the eviction journey gives the child, far below the bytes
/// the log's frames need, so the window cannot keep the front of the transcript
/// while the tail is on view.
const EVICTION_BUDGET_BYTES: usize = 64 * 1024;

/// How far above the tail the park journey moves, a few screens back: past the
/// settle marker, still inside the loaded rows.
const PARK_PRESSES: usize = 6;

/// How long a press has to show its effect: long enough that a scheduler shared
/// with two other suites cannot make a landed press look like a lost one.
const PRESS_WINDOW: Duration = Duration::from_secs(3);

/// How many presses the walk back through the history takes. A read landing
/// above the view pushes it back down, so the walk needs room for that.
const WALK_PRESSES: usize = 200;

/// The presses of the walk that have to land. A view whose rows ran out behind
/// it stops moving, and the floor sits under what a working walk reaches.
const WALK_MIN_MOVES: usize = 150;

/// How far below the held run's resident bytes the evicting run has to stay:
/// under the gap the two budgets leave, above the allocation difference between
/// two runs of one binary.
const EVICTION_RSS_MARGIN_KB: u64 = 2 * 1024;

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
/// The line number of the status row moving is the press landing, and the
/// frame is returned only once a second reading agrees with it, because the
/// terminal carries the rows of the frame being painted and a reading taken
/// mid-paint pairs a new status row with rows that are still the frame before
/// it. The top row's text is no landing test: the view can stand on a blank
/// row, so two positions whose top rows are both blank read as one place. The
/// status row carries its position and its row count as numbers, and being
/// painted last it changes only once the rows above it are the new frame's.
/// The caller parks on the returned frame, so the row it stands on and the
/// line number beside it belong to one frame.
fn press_page_up(session: &mut PtySession) -> Option<String> {
    let before = view_line(&session.screen().contents());
    session.send_key(&Key::PageUp);
    let deadline = Instant::now() + PRESS_WINDOW;
    let mut settled: Option<String> = None;
    while Instant::now() < deadline {
        let now = session.screen().contents();
        let landed = view_line(&now).is_some_and(|line| Some(line) != before);
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

/// What a walk up the history reached: the total the status published at its
/// first landed press, the deepest total and line the view stood on, the top
/// row there, and how many presses landed.
#[derive(Default)]
struct WalkOutcome {
    first_total: u64,
    deepest_total: u64,
    deepest_line: u64,
    deepest_top: String,
    presses: usize,
}

/// Press up a fixed number of times, reporting what the walk reached.
///
/// Every reading belongs to one frame, since a press reports the frame it
/// settled on. A press whose landing row the window no longer holds reads as
/// no movement until the read of the log lands, so the walk asks again instead
/// of calling the view's place lost; a view that stops moving twice in a row
/// has no rows left above it, and the walk ends there rather than waiting out
/// the remaining presses.
fn walk_up(session: &mut PtySession) -> WalkOutcome {
    let mut outcome = WalkOutcome::default();
    let mut idle = 0;
    for _ in 0..WALK_PRESSES {
        let Some(frame) = press_page_up(session) else {
            idle += 1;
            if idle >= 2 {
                break;
            }
            continue;
        };
        idle = 0;
        outcome.presses += 1;
        if let Some(total) = view_total(&frame) {
            if outcome.first_total == 0 {
                outcome.first_total = total;
            }
            outcome.deepest_total = outcome.deepest_total.max(total);
        }
        if let Some(line) = view_line(&frame)
            && (outcome.deepest_line == 0 || line < outcome.deepest_line)
        {
            outcome.deepest_line = line;
            outcome.deepest_top = top_row(&frame).to_string();
        }
    }
    outcome
}

/// A run whose resident window is smaller than its log still renders the tail,
/// still walks back to the oldest row its window keeps, and reports a shorter
/// history and fewer resident bytes than a run whose window keeps the log.
///
/// Both legs read the same log at two budgets and walk the same history. The
/// evicting run's window is far below what the log's frames need, so its front
/// leaves while the tail is on view and the rows above it come back from the
/// log. The walk lands in both legs, since either window is served by the same
/// read path, so what separates them is what each reports: the row total is the
/// history that window kept, and the resident bytes are what the two windows
/// the variable drives release together — the journey measures the two
/// together and does not separate them.
///
/// A run given the eviction budget whose walk stops before the top of the rows
/// it kept is the run this reads as broken: the reader would have nothing above
/// it, which is what a window that lost the rows it gave up looks like from the
/// screen.
#[test]
#[ignore = "PTY journey; the real binary replays a seeded log"]
fn test_smaller_budget_drains_front() {
    let dir = fresh_temp_dir("journey-evict");
    let sid = "88888888-8888-8888-8888-888888888888";
    let fixture = seed_large_session_on_disk(&dir, sid, "measure-model", JOURNEY_FIXTURE_BYTES);

    let (mut held, _) = launch_and_settle(&dir, &fixture);
    let held_rss = sample_rss_kb(held.child_pid().expect("child pid"));
    let held_outcome = walk_up(&mut held);
    held.kill_hard();
    assert!(held.wait_for_exit(RENDER_TIMEOUT), "the held run exits");

    let budget = [(
        ENV_HOUYICODER_TEST_RESIDENT_BYTE_BUDGET.to_string(),
        EVICTION_BUDGET_BYTES.to_string(),
    )];
    let (mut evicted, _) = launch_and_settle_with(&dir, &fixture, &budget);
    let evicted_rss = sample_rss_kb(evicted.child_pid().expect("child pid"));
    let evicted_outcome = walk_up(&mut evicted);
    println!(
        "resident budget eviction: held {} presses, tail {}/{} to line {} of {}, rss \
         {held_rss:?} KiB; evicted {} presses, tail {}/{} to line {} of {} behind {:?}, rss \
         {evicted_rss:?} KiB",
        held_outcome.presses,
        held_outcome.first_total,
        held_outcome.deepest_total,
        held_outcome.deepest_line,
        held_outcome.first_total,
        evicted_outcome.presses,
        evicted_outcome.first_total,
        evicted_outcome.deepest_total,
        evicted_outcome.deepest_line,
        evicted_outcome.first_total,
        evicted_outcome.deepest_top,
    );

    assert!(
        held_outcome.presses >= WALK_MIN_MOVES && held_outcome.deepest_line == 1,
        "the run whose window keeps the log walks its history to the oldest row it kept: {} of \
         {} presses moved the view and the deepest line was {}\n{}",
        held_outcome.presses,
        WALK_PRESSES,
        held_outcome.deepest_line,
        held.output()
    );
    assert!(
        evicted_outcome.presses >= WALK_MIN_MOVES,
        "the bounded window keeps moving through the rows above it: {} of {} presses moved the \
         view:\n{}",
        evicted_outcome.presses,
        WALK_PRESSES,
        evicted.output()
    );
    assert_eq!(
        evicted_outcome.deepest_line,
        1,
        "the reader reaches the oldest row its window kept, {} rows above the tail it settled \
         on; \
         the deepest line was {} of {} behind {:?}\n{}",
        evicted_outcome.first_total.saturating_sub(1),
        evicted_outcome.deepest_line,
        evicted_outcome.deepest_total,
        evicted_outcome.deepest_top,
        evicted.output()
    );
    assert!(
        evicted_outcome.deepest_total < held_outcome.deepest_total,
        "the smaller window reports a shorter history than the run that keeps the log: {} rows \
         against {} on the same log\n{}",
        evicted_outcome.deepest_total,
        held_outcome.deepest_total,
        evicted.output()
    );
    let (Some(held_rss), Some(evicted_rss)) = (held_rss, evicted_rss) else {
        panic!("both runs report their resident bytes");
    };
    assert!(
        evicted_rss + EVICTION_RSS_MARGIN_KB <= held_rss,
        "the smaller window costs less memory: held {held_rss} KiB against evicted {evicted_rss} KiB"
    );
}
