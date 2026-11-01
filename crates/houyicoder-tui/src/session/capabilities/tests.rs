//! Tests for the capability caches.

use houyicoder_protocol::frontend::tasks::{
    BackgroundTaskId, BackgroundTaskSnapshot, BackgroundTaskStatus,
};

use super::*;

fn snapshot(id: u64, status: BackgroundTaskStatus, elapsed_ms: u64) -> BackgroundTaskSnapshot {
    BackgroundTaskSnapshot {
        id: BackgroundTaskId(id),
        status,
        command: format!("job {id}"),
        elapsed_ms,
        exit_code: None,
    }
}

/// A frame that still reports a task running after its ending arrived is
/// older than what the cache holds, so the row keeps the ending the user
/// already read.
#[test]
fn test_record_keeps_ending() {
    let mut tasks = TasksCapability::default();
    tasks.record(snapshot(1, BackgroundTaskStatus::Finished, 9_000));
    tasks.record(snapshot(1, BackgroundTaskStatus::Running, 4_000));
    assert_eq!(
        tasks.get(BackgroundTaskId(1)).map(|task| &task.status),
        Some(&BackgroundTaskStatus::Finished)
    );
    assert_eq!(tasks.running_count(), 0);
}

/// A listing read before an ending landed carries the older running state,
/// so the held ending wins: the task never falls back from ended to running.
#[test]
fn test_list_keeps_ending() {
    let mut tasks = TasksCapability::default();
    tasks.record(snapshot(1, BackgroundTaskStatus::Finished, 9_000));
    tasks.replace_all(vec![snapshot(1, BackgroundTaskStatus::Running, 4_000)]);
    assert_eq!(
        tasks.get(BackgroundTaskId(1)).map(|task| &task.status),
        Some(&BackgroundTaskStatus::Finished)
    );
    assert_eq!(tasks.running_count(), 0);
}

/// A task the listing does not hold is gone from the host, so the cache
/// drops it rather than keeping a row nothing answers for.
#[test]
fn test_list_drops_missing() {
    let mut tasks = TasksCapability::default();
    tasks.record(snapshot(1, BackgroundTaskStatus::Finished, 9_000));
    tasks.record(snapshot(2, BackgroundTaskStatus::Running, 4_000));
    tasks.replace_all(vec![snapshot(2, BackgroundTaskStatus::Running, 7_000)]);
    assert!(tasks.get(BackgroundTaskId(1)).is_none());
    assert_eq!(
        tasks.get(BackgroundTaskId(2)).map(|task| task.elapsed_ms),
        Some(7_000),
        "a running task takes the listing's newer reading"
    );
}
