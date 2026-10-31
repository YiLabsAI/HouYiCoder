//! The direct shell command's replies: its own reply settles it, a reply for
//! another request leaves it running, and a lost connection ends it.

use super::*;
use crate::test_harness::{connected_app_events, render_text, wait_for_request};

/// The host answers a finished command with Ack, and that reply is what
/// clears the running state the status bar advertises.
#[test]
fn test_ack_clears_shell() {
    let (mut app, events) = connected_app_events();
    assert!(app.submit_direct_shell("echo hi".into()));
    let env = wait_for_request(&events, |payload| {
        matches!(payload, FrontendRequest::DirectShellRun { .. })
    });
    assert!(app.shell_run_active());
    app.handle_agent_message(SessionMessage::Response {
        request: env.req_id,
        response: ServerResponse::Ack,
    });
    assert!(
        !app.shell_run_active(),
        "the command's own reply ends it, so the hint clears itself"
    );
}

/// A reply that answers a different request must not clear the command: the
/// request id is what tells two commands apart.
#[test]
fn test_stale_ack_leaves_running() {
    let (mut app, events) = connected_app_events();
    assert!(app.submit_direct_shell("echo hi".into()));
    let env = wait_for_request(&events, |payload| {
        matches!(payload, FrontendRequest::DirectShellRun { .. })
    });
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(env.req_id.0 + 1000),
        response: ServerResponse::Ack,
    });
    assert!(
        app.shell_run_active(),
        "an unrelated reply leaves the running command alone"
    );
}

/// A refusal the host reports ends the command and prints the cause, so the
/// user learns why nothing ran instead of watching a hint that never clears.
#[test]
fn test_error_reports_and_clears() {
    let (mut app, events) = connected_app_events();
    assert!(app.submit_direct_shell("echo hi".into()));
    let env = wait_for_request(&events, |payload| {
        matches!(payload, FrontendRequest::DirectShellRun { .. })
    });
    app.handle_agent_message(SessionMessage::Response {
        request: env.req_id,
        response: ServerResponse::Error {
            message: "direct shell: no bash tool is registered".into(),
        },
    });
    assert!(!app.shell_run_active());
    let screen = render_text(&app, 80, 24);
    assert!(
        screen.contains("no bash tool is registered"),
        "the host's cause reaches the transcript: {screen}"
    );
}

/// A refusal that answers a different request leaves the running command
/// alone: only the request id it was submitted with may end it, so an older
/// request's error cannot clear a newer command.
#[test]
fn test_stale_error_leaves_running() {
    let (mut app, events) = connected_app_events();
    assert!(app.submit_direct_shell("echo hi".into()));
    let env = wait_for_request(&events, |payload| {
        matches!(payload, FrontendRequest::DirectShellRun { .. })
    });
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(env.req_id.0 + 1000),
        response: ServerResponse::Error {
            message: "an unrelated request failed".into(),
        },
    });
    assert!(
        app.shell_run_active(),
        "an unrelated refusal leaves the running command alone"
    );
}

/// The loss distinguishes a command the driver can prove never reached the
/// transport from one that may have run: the transcript says not sent rather
/// than claiming an interruption of work that never started.
#[test]
fn test_loss_not_sent_shell() {
    let (mut app, events) = connected_app_events();
    assert!(app.submit_direct_shell("sleep 30".into()));
    let env = wait_for_request(&events, |payload| {
        matches!(payload, FrontendRequest::DirectShellRun { .. })
    });
    app.apply_connection_loss("server closed the connection".into(), vec![env.req_id]);
    assert!(!app.shell_run_active());
    let screen = render_text(&app, 80, 24);
    assert!(
        screen.contains("shell command not sent"),
        "the loss reports the command never left the process: {screen}"
    );
    assert!(
        !screen.contains("agent error"),
        "the command already named the loss, so no run is blamed: {screen}"
    );
}

/// A dead transport can never answer the command, so the loss clears it
/// rather than leaving the status bar advertising work that is gone.
#[test]
fn test_loss_clears_shell() {
    let (mut app, _events) = connected_app_events();
    assert!(app.submit_direct_shell("sleep 30".into()));
    app.apply_connection_loss("server closed the connection".into(), Vec::new());
    assert!(!app.shell_run_active());
    let screen = render_text(&app, 80, 24);
    assert!(
        screen.contains("shell command interrupted"),
        "the loss is reported as the command's end: {screen}"
    );
    assert!(
        !screen.contains("agent error"),
        "the command already named the loss, so no run is blamed: {screen}"
    );
}
