//! Routing contracts for the background-task requests: the list a tasks pane
//! opens with, one task's detail, and the kill that stops a running command.
//!
//! The registry's own state machine is covered beside the registry; these
//! tests drive the requests through the server, so what they assert is the
//! reply a frontend actually reads.

#![cfg(test)]

use super::super::*;
use super::dispatch_response::dispatch_responses;
use crate::composition::background_tasks::BackgroundTasks;
use crate::composition::background_tasks::test_support::wait_until;
use houyicoder_api::background::{BackgroundCommandLauncher, CommandTail, DetachedCommand};
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::ResponsePayload;
use houyicoder_protocol::error::ErrorCategory;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::tasks::{
    BackgroundTaskId, BackgroundTaskSnapshot, BackgroundTaskStatus,
};
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use std::future::pending;
use std::sync::Arc;
use std::time::Instant;

/// A host whose runner holds no tools, optionally owning the session's task
/// registry. None is the shape no command can run on: the list still answers.
fn server(tasks: Option<Arc<BackgroundTasks>>) -> Server {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Runner::new(
        store,
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    let server = Server::new(
        Arc::new(runner),
        SessionId::new(),
        Arc::new(DefaultModeGate::new()),
    );
    match tasks {
        Some(tasks) => server.with_background_tasks(tasks),
        None => server,
    }
}

/// Register a command that runs until the test stops it, standing in for a
/// shell the tool handed to the background.
fn launch(tasks: &BackgroundTasks, command: &str) -> BackgroundTaskId {
    tasks.launch(DetachedCommand {
        command: command.into(),
        started_at: Instant::now(),
        tail: CommandTail::new(),
        completion: Box::pin(pending()),
    })
}

fn commands(list: &[BackgroundTaskSnapshot]) -> Vec<&str> {
    list.iter().map(|task| task.command.as_str()).collect()
}

/// The list carries every task the registry holds, in the order the host
/// assigned the numbers, so the pane opens on the commands that exist now.
#[tokio::test]
async fn test_list_reports_tasks() {
    let tasks = Arc::new(BackgroundTasks::new());
    launch(&tasks, "cargo build");
    launch(&tasks, "cargo test");
    let payloads = dispatch_responses(
        server(Some(tasks)),
        vec![FrontendRequest::ListBackgroundTasks],
    )
    .await;
    match &payloads[0] {
        ResponsePayload::BackgroundTasks(list) => {
            assert_eq!(
                commands(list),
                vec!["cargo build", "cargo test"],
                "both tasks are listed in assignment order"
            );
            assert!(
                list.iter()
                    .all(|task| task.status == BackgroundTaskStatus::Running),
                "both are still running: {list:?}"
            );
        }
        other => panic!("expected BackgroundTasks, got {other:?}"),
    }
}

/// A host assembled without a registry answers an empty list, because no
/// command can be running on it; the pane opens empty instead of reporting
/// a failure the user cannot act on.
#[tokio::test]
async fn test_list_empty_without_registry() {
    let payloads =
        dispatch_responses(server(None), vec![FrontendRequest::ListBackgroundTasks]).await;
    match &payloads[0] {
        ResponsePayload::BackgroundTasks(list) => {
            assert!(list.is_empty(), "nothing runs: {list:?}")
        }
        other => panic!("expected an empty BackgroundTasks, got {other:?}"),
    }
}

/// The detail answers for one number with the record the pane shows: the
/// command and the state the host holds for it.
#[tokio::test]
async fn test_detail_returns_record() {
    let tasks = Arc::new(BackgroundTasks::new());
    let id = launch(&tasks, "cargo build");
    let payloads = dispatch_responses(
        server(Some(tasks)),
        vec![FrontendRequest::BackgroundTaskDetail { id }],
    )
    .await;
    match &payloads[0] {
        ResponsePayload::BackgroundTaskDetail(record) => {
            assert_eq!(record.id, id, "the record answers for the number asked");
            assert_eq!(record.command, "cargo build", "the command is named");
            assert_eq!(
                record.status,
                BackgroundTaskStatus::Running,
                "the task is still running"
            );
        }
        other => panic!("expected BackgroundTaskDetail, got {other:?}"),
    }
}

/// A number the host never assigned is refused as an invalid request: the
/// caller asked for a task it believes is running here, and the reply must
/// say the host holds no such command.
#[tokio::test]
async fn test_detail_unknown_refused() {
    let tasks = Arc::new(BackgroundTasks::new());
    let payloads = dispatch_responses(
        server(Some(tasks)),
        vec![FrontendRequest::BackgroundTaskDetail {
            id: BackgroundTaskId(9),
        }],
    )
    .await;
    match &payloads[0] {
        ResponsePayload::Error(err) => {
            assert_eq!(err.category, ErrorCategory::InvalidRequest);
            assert!(
                err.message.contains("no background task #9"),
                "the refusal names the number: {}",
                err.message
            );
        }
        other => panic!("expected an Error, got {other:?}"),
    }
}

/// A host assembled without a registry holds no task, so a detail request
/// for any number names one it never had.
#[tokio::test]
async fn test_detail_unknown_without_registry() {
    let payloads = dispatch_responses(
        server(None),
        vec![FrontendRequest::BackgroundTaskDetail {
            id: BackgroundTaskId(3),
        }],
    )
    .await;
    match &payloads[0] {
        ResponsePayload::Error(err) => {
            assert_eq!(err.category, ErrorCategory::InvalidRequest);
            assert!(
                err.message.contains("no background task #3"),
                "the refusal names the number: {}",
                err.message
            );
        }
        other => panic!("expected an Error, got {other:?}"),
    }
}

/// A kill takes effect on the task the list reports, so the pane's next read
/// shows the command stopped rather than still running.
#[tokio::test]
async fn test_kill_stops_task() {
    let tasks = Arc::new(BackgroundTasks::new());
    let id = launch(&tasks, "cargo build");
    let reply = dispatch_responses(
        server(Some(Arc::clone(&tasks))),
        vec![FrontendRequest::KillBackgroundTask { id }],
    )
    .await;
    assert!(
        matches!(&reply[0], ResponsePayload::Ack),
        "a kill that took effect acknowledges: {:?}",
        reply[0]
    );
    wait_until(|| {
        tasks
            .detail(id)
            .is_some_and(|task| task.status == BackgroundTaskStatus::Killed)
    })
    .await;
    let listed = dispatch_responses(
        server(Some(tasks)),
        vec![FrontendRequest::ListBackgroundTasks],
    )
    .await;
    match &listed[0] {
        ResponsePayload::BackgroundTasks(list) => assert_eq!(
            list[0].status,
            BackgroundTaskStatus::Killed,
            "the list reports the stop the kill made: {list:?}"
        ),
        other => panic!("expected BackgroundTasks, got {other:?}"),
    }
}

/// Killing a task that already ended is a different refusal from killing a
/// number the host never had: there is nothing left to stop, and the user
/// reads that rather than a missing-task error.
#[tokio::test]
async fn test_kill_finished_refused() {
    let tasks = Arc::new(BackgroundTasks::new());
    let id = launch(&tasks, "cargo build");
    assert!(tasks.kill(id), "the test stops the task first");
    wait_until(|| {
        tasks
            .detail(id)
            .is_some_and(|task| task.status.is_terminal())
    })
    .await;
    let payloads = dispatch_responses(
        server(Some(tasks)),
        vec![FrontendRequest::KillBackgroundTask { id }],
    )
    .await;
    match &payloads[0] {
        ResponsePayload::Error(err) => {
            assert_eq!(err.category, ErrorCategory::InvalidRequest);
            assert!(
                err.message.contains("is not running"),
                "the refusal says the task has nothing left to stop: {}",
                err.message
            );
        }
        other => panic!("expected an Error, got {other:?}"),
    }
}

/// A kill for a number the host never assigned is refused as the unknown
/// task it is, not as a task that already ended.
#[tokio::test]
async fn test_kill_unknown_refused() {
    let tasks = Arc::new(BackgroundTasks::new());
    let payloads = dispatch_responses(
        server(Some(tasks)),
        vec![FrontendRequest::KillBackgroundTask {
            id: BackgroundTaskId(7),
        }],
    )
    .await;
    match &payloads[0] {
        ResponsePayload::Error(err) => {
            assert_eq!(err.category, ErrorCategory::InvalidRequest);
            assert!(
                err.message.contains("no background task #7"),
                "the refusal names the missing number: {}",
                err.message
            );
        }
        other => panic!("expected an Error, got {other:?}"),
    }
}

/// A host assembled without a registry refuses a kill for any number: it
/// holds no task, and claiming a stop it cannot perform would be worse.
#[tokio::test]
async fn test_kill_unknown_without_registry() {
    let payloads = dispatch_responses(
        server(None),
        vec![FrontendRequest::KillBackgroundTask {
            id: BackgroundTaskId(5),
        }],
    )
    .await;
    match &payloads[0] {
        ResponsePayload::Error(err) => {
            assert_eq!(err.category, ErrorCategory::InvalidRequest);
            assert!(
                err.message.contains("no background task #5"),
                "the refusal names the missing number: {}",
                err.message
            );
        }
        other => panic!("expected an Error, got {other:?}"),
    }
}
