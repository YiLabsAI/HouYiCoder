//! The task list a reply carries: the status bar counts what the reply holds,
//! and a later reply replaces the whole set.

use super::*;
use crate::state::App;
use crate::test_harness::{connected_app_events, render_text};
use houyicoder_protocol::frontend::tasks::{
    BackgroundTaskId, BackgroundTaskSnapshot, BackgroundTaskStatus,
};

fn running(id: u64, command: &str) -> BackgroundTaskSnapshot {
    BackgroundTaskSnapshot {
        id: BackgroundTaskId(id),
        status: BackgroundTaskStatus::Running,
        command: command.into(),
        elapsed_ms: 250,
        exit_code: None,
    }
}

fn apply_tasks(app: &mut App, tasks: Vec<BackgroundTaskSnapshot>) {
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(1),
        response: ServerResponse::BackgroundTasks { tasks },
    });
}

/// The status bar counts the running tasks the reply carries, so the user
/// learns a command is still working without opening the pane.
#[test]
fn test_tasks_reply_shows_count() {
    let (mut app, _events) = connected_app_events();
    apply_tasks(&mut app, vec![running(1, "cargo build")]);
    let screen = render_text(&app, 120, 24);
    assert!(
        screen.contains("1 background task \u{00b7} /tasks"),
        "the status bar counts the running task: {screen}"
    );
}

/// A later reply replaces the set rather than adding to it: the count follows
/// the newest snapshot, so a task the host no longer holds stops being
/// counted.
#[test]
fn test_tasks_reply_replaces_set() {
    let (mut app, _events) = connected_app_events();
    apply_tasks(
        &mut app,
        vec![running(1, "cargo build"), running(2, "cargo test")],
    );
    assert!(
        render_text(&app, 120, 24).contains("2 background tasks"),
        "both tasks are counted first"
    );
    apply_tasks(&mut app, vec![running(3, "cargo fmt")]);
    let screen = render_text(&app, 120, 24);
    assert!(
        screen.contains("1 background task"),
        "the replaced set counts one task: {screen}"
    );
}

/// A reply holding no tasks clears the count, so the hint disappears once
/// every command ended instead of counting tasks the host dropped.
#[test]
fn test_tasks_reply_clears() {
    let (mut app, _events) = connected_app_events();
    apply_tasks(&mut app, vec![running(1, "cargo build")]);
    assert!(
        render_text(&app, 120, 24).contains("/tasks"),
        "the hint is shown while a task runs"
    );
    apply_tasks(&mut app, Vec::new());
    let screen = render_text(&app, 120, 24);
    assert!(
        !screen.contains("/tasks"),
        "the hint clears with the empty set: {screen}"
    );
}
