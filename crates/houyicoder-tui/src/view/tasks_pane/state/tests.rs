use std::time::{Duration, Instant};

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::tasks::{
    BackgroundTaskDetail, BackgroundTaskId, BackgroundTaskStatus,
};

use super::TasksPaneState;

fn record(id: u64, status: BackgroundTaskStatus) -> BackgroundTaskDetail {
    BackgroundTaskDetail {
        id: BackgroundTaskId(id),
        status,
        command: "cargo build".into(),
        elapsed_ms: 5_000,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        tail: Vec::new(),
    }
}

/// The cursor stays inside the rows the list shows: it does not run past the
/// last task, below the first, or into an empty list.
#[test]
fn test_cursor_clamps_to_rows() {
    let pane = TasksPaneState::default();
    pane.move_cursor(-1, 3);
    assert_eq!(pane.cursor(), 0, "the cursor cannot go above the first row");
    pane.move_cursor(10, 3);
    assert_eq!(pane.cursor(), 2, "the cursor cannot run past the last row");
    pane.move_cursor(1, 0);
    assert_eq!(pane.cursor(), 0, "an empty list has no row to point at");
}

/// The action acts on the open detail when one is open, and on the selected
/// row otherwise, so Enter and k always hit the task the user is looking at.
#[test]
fn test_target_follows_open_detail() {
    let rows = [BackgroundTaskId(1), BackgroundTaskId(2)];
    let mut pane = TasksPaneState::default();
    pane.move_cursor(1, rows.len());
    assert_eq!(pane.target(&rows), Some(BackgroundTaskId(2)));
    pane.open_detail(BackgroundTaskId(1), RequestId(7), Instant::now());
    assert_eq!(
        pane.target(&rows),
        Some(BackgroundTaskId(1)),
        "the open detail outranks the list cursor"
    );
    assert_eq!(pane.target(&[]), Some(BackgroundTaskId(1)));
    pane.close_detail();
    assert_eq!(
        pane.target(&[]),
        None,
        "an empty list has no task to act on"
    );
}

/// A reply to an earlier read is refused, so a slower read cannot replace a
/// newer record or reopen a detail the user closed.
#[test]
fn test_stale_reply_ignored() {
    let mut pane = TasksPaneState::default();
    let now = Instant::now();
    pane.open_detail(BackgroundTaskId(3), RequestId(1), now);
    assert!(
        !pane.apply_detail(RequestId(2), record(3, BackgroundTaskStatus::Running)),
        "a reply for another read does not fill the detail"
    );
    assert!(pane.detail_record().is_none());
    assert!(
        pane.apply_detail(RequestId(1), record(3, BackgroundTaskStatus::Running)),
        "the reply matching the open read fills it"
    );
    pane.close_detail();
    assert!(
        !pane.apply_detail(RequestId(1), record(3, BackgroundTaskStatus::Finished)),
        "a reply arriving after the detail closed has nowhere to land"
    );
}

/// The detail reads again only once the previous read answered and its task
/// still runs; an ended task, or one whose read is out, is not due.
#[test]
fn test_detail_reads_until_ended() {
    let mut pane = TasksPaneState::default();
    let start = Instant::now();
    pane.open_detail(BackgroundTaskId(2), RequestId(1), start);
    assert_eq!(
        pane.detail_due(start + Duration::from_secs(5)),
        None,
        "no second read while the first is out"
    );
    let reply = start + Duration::from_secs(1);
    assert!(pane.apply_detail(RequestId(1), record(2, BackgroundTaskStatus::Running)));
    assert_eq!(
        pane.detail_due(reply),
        Some(BackgroundTaskId(2)),
        "the next read is due once the previous one answered"
    );
    pane.mark_detail_read(RequestId(2), reply);
    assert_eq!(pane.detail_due(reply), None, "the read just went out");
    assert!(pane.apply_detail(RequestId(2), record(2, BackgroundTaskStatus::Finished)));
    assert_eq!(
        pane.detail_due(reply + Duration::from_secs(30)),
        None,
        "an ended task has no further state to read"
    );
}

/// The list reads on its own clock while the pane is open, and stopping it
/// leaves the pane quiet.
#[test]
fn test_list_read_schedule() {
    let mut pane = TasksPaneState::default();
    let start = Instant::now();
    pane.begin_list_read(start);
    assert!(!pane.list_due(start), "the open path already asked once");
    assert!(pane.list_due(start + Duration::from_secs(1)));
    pane.mark_list_read(start + Duration::from_secs(1));
    assert!(!pane.list_due(start + Duration::from_secs(1)));
    assert!(pane.list_due(start + Duration::from_secs(2)));
    pane.stop_list_read();
    assert!(!pane.list_due(start + Duration::from_secs(60)));
}

/// A lost connection settles the pending read and drops both schedules, so
/// the pane stops retrying against a transport that cannot answer.
#[test]
fn test_clear_pending_stops_reads() {
    let mut pane = TasksPaneState::default();
    let start = Instant::now();
    pane.begin_list_read(start);
    pane.open_detail(BackgroundTaskId(4), RequestId(9), start);
    pane.clear_pending();
    assert_eq!(
        pane.detail_due(start + Duration::from_secs(60)),
        None,
        "the detail stops reading"
    );
    assert!(
        !pane.list_due(start + Duration::from_secs(60)),
        "the list stops reading"
    );
    assert!(
        !pane.apply_detail(RequestId(9), record(4, BackgroundTaskStatus::Running)),
        "the settled read no longer matches a reply"
    );
}

/// Scrolling stops at the offset the last render measured, so a short record
/// cannot leave the view past its last line.
#[test]
fn test_scroll_clamps_to_end() {
    let mut pane = TasksPaneState::default();
    pane.open_detail(BackgroundTaskId(5), RequestId(1), Instant::now());
    pane.scroll_detail(5);
    assert_eq!(pane.offset(), 0, "nothing measured yet, nothing to scroll");
    pane.set_detail_max_offset(10);
    pane.scroll_detail(5);
    assert_eq!(pane.offset(), 5);
    pane.scroll_detail(-3);
    assert_eq!(pane.offset(), 2);
    pane.scroll_detail(-100);
    assert_eq!(pane.offset(), 0);
    pane.set_detail_max_offset(3);
    assert_eq!(pane.offset(), 0, "a shorter record pulls the view back");
}
