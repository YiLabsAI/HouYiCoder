use houyicoder_protocol::frontend::tasks::{
    BackgroundTaskId, BackgroundTaskSnapshot, BackgroundTaskStatus,
};

use super::{TaskRow, task_rows};

fn snapshot(id: u64, status: BackgroundTaskStatus) -> BackgroundTaskSnapshot {
    BackgroundTaskSnapshot {
        id: BackgroundTaskId(id),
        status,
        command: format!("command {id}"),
        elapsed_ms: 1_000,
        exit_code: None,
    }
}

fn ids(rows: &[TaskRow<'_>]) -> Vec<u64> {
    rows.iter().map(|row| row.task().id.0).collect()
}

/// A live command is listed above every finished one, whatever numbers they
/// carry, so the row the pane can still act on is the first one read.
#[test]
fn test_rows_list_running_first() {
    let tasks = [
        snapshot(1, BackgroundTaskStatus::Running),
        snapshot(2, BackgroundTaskStatus::Finished),
        snapshot(3, BackgroundTaskStatus::Running),
        snapshot(4, BackgroundTaskStatus::Killed),
    ];
    let rows = task_rows(tasks.iter());
    assert_eq!(ids(&rows), vec![3, 1, 4, 2]);
    assert_eq!(rows[0].section(), "Running");
    assert_eq!(rows[2].section(), "Ended");
}

/// Within a section the newest number comes first, so the most recent work is
/// at the top of both lists.
#[test]
fn test_rows_order_newest_first() {
    let tasks = [
        snapshot(7, BackgroundTaskStatus::Running),
        snapshot(9, BackgroundTaskStatus::Running),
        snapshot(2, BackgroundTaskStatus::Failed),
        snapshot(5, BackgroundTaskStatus::TimedOut),
    ];
    assert_eq!(ids(&task_rows(tasks.iter())), vec![9, 7, 5, 2]);
}

/// A state this build does not name is not an ending: the row stays in the
/// running list and keeps the word it received.
#[test]
fn test_unknown_status_stays_running() {
    let tasks = [snapshot(1, BackgroundTaskStatus::Unknown("paused".into()))];
    let rows = task_rows(tasks.iter());
    assert_eq!(rows[0].section(), "Running");
}

/// Every named ending shares one section, and the row order inside it does
/// not follow how the commands ended.
#[test]
fn test_ended_rows_share_section() {
    let tasks = [
        snapshot(1, BackgroundTaskStatus::Finished),
        snapshot(2, BackgroundTaskStatus::Failed),
        snapshot(3, BackgroundTaskStatus::Killed),
        snapshot(4, BackgroundTaskStatus::TimedOut),
    ];
    let rows = task_rows(tasks.iter());
    assert!(rows.iter().all(|row| row.section() == "Ended"));
    assert_eq!(ids(&rows), vec![4, 3, 2, 1]);
}
