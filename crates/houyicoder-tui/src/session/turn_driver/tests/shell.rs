//! Direct shell submission: one command at a time, refused while the host is
//! busy, and a refusal that costs the user nothing.

use super::*;
use crate::test_harness::connected_app_events;

/// The host runs one operation at a time on one cancel slot, so a command
/// submitted while the agent is working is refused. The refusal names the
/// wait, and nothing starts running.
#[test]
fn test_shell_refused_when_busy() {
    let (mut app, _events) = connected_app_events();
    app.sessions.current.run.start(RequestId(1), Instant::now());
    assert!(!app.submit_direct_shell("echo hi".into()));
    assert!(
        !app.shell_run_active(),
        "a refused command must not claim the shell state"
    );
    let screen = render_text(&app, 80, 24);
    assert!(
        screen.contains("the agent is working"),
        "the refusal names the run holding the host: {screen}"
    );
}

/// A second command is refused while the first runs, and the running command
/// keeps the request id its reply will answer.
#[test]
fn test_second_shell_refused() {
    let (mut app, _events) = connected_app_events();
    assert!(app.submit_direct_shell("sleep 30".into()));
    let first = app.sessions.current.shell.request_id();
    assert!(!app.submit_direct_shell("echo hi".into()));
    assert_eq!(
        app.sessions.current.shell.request_id(),
        first,
        "the running command keeps its own request"
    );
}

/// A turn submitted while a command runs is refused instead of mirroring into
/// the host, and the draft stays in the composer.
#[test]
fn test_shell_blocks_message_submit() {
    let (mut app, _events) = connected_app_events();
    assert!(app.submit_direct_shell("sleep 30".into()));
    app.ui.input.editor.set("what changed?".into());
    app.submit_input();
    assert!(
        !app.sessions.current.run.is_active(),
        "no turn starts while the command holds the host"
    );
    assert_eq!(
        app.ui.input.editor.value(),
        "what changed?",
        "the refused message stays in the composer"
    );
}

/// Compaction cannot run while a command holds the host: the refusal names
/// the command, so the user knows what to stop rather than waiting.
#[test]
fn test_compact_refused_during_shell() {
    let (mut app, _events) = connected_app_events();
    assert!(app.submit_direct_shell("sleep 30".into()));
    app.run_compact();
    let screen = render_text(&app, 80, 24);
    assert!(
        screen.contains("a shell command is running"),
        "the refusal names the running command: {screen}"
    );
}
