use crate::agent_message::{ServerResponse, SessionMessage};
use crate::records::TranscriptLine;
use crate::state::App;
use crate::test_harness::connection_lost_app;
use crate::test_harness::working_app;
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::debug::DebugState;

/// /debug with no backend wired (stub path, no session) surfaces a
/// system line rather than panicking, for both argument forms. This is
/// the path a test-only App without a runner takes. The level the
/// command carries is proven by the frame-level tests over the driver.
#[test]
fn test_no_session_not_connected() {
    let mut app = working_app();
    app.run_debug("");
    assert_system_not_connected(&app);
    app.run_debug("off");
    assert_system_not_connected(&app);
}

fn assert_system_not_connected(app: &App) {
    let last = app
        .sessions
        .current
        .transcript
        .last()
        .expect("a line was pushed");
    match last {
        TranscriptLine::System(text) => {
            assert!(
                text.contains("not connected"),
                "expected not-connected, got {text}"
            );
        }
        other => panic!("expected a system line, got {other:?}"),
    }
}

/// /debug dispatched via the string command parser (typing /debug in the
/// input box) reaches run_debug, same as the direct call above.
#[test]
fn test_debug_dispatched_via_command() {
    let mut app = working_app();
    assert!(app.run_tui_local_command("debug"));
    assert!(app.run_tui_local_command("debug off"));
}

/// A Debug reply from the server renders as a system line the
/// user sees: the path when enabled, "logging off" when disabled. This
/// is what tells the user where to look — the whole point of carrying
/// the path in the reply.
#[test]
fn test_debug_result_renders_path() {
    let mut app = working_app();
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(1),
        response: ServerResponse::Debug {
            state: DebugState {
                enabled: true,
                path: "/tmp/test.log".into(),
            },
        },
    });
    let last = app
        .sessions
        .current
        .transcript
        .last()
        .expect("a line was pushed");
    match last {
        TranscriptLine::System(text) => {
            assert!(
                text.contains("/tmp/test.log"),
                "the path should be in the system line, got {text}"
            );
        }
        other => panic!("expected a system line, got {other:?}"),
    }

    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(2),
        response: ServerResponse::Debug {
            state: DebugState {
                enabled: false,
                path: String::new(),
            },
        },
    });
    let last = app
        .sessions
        .current
        .transcript
        .last()
        .expect("a line was pushed");
    match last {
        TranscriptLine::System(text) => {
            assert!(
                text.contains("off"),
                "the off line should say off, got {text}"
            );
        }
        other => panic!("expected a system line, got {other:?}"),
    }
}

/// A dead driver refuses the level change: the refusal names the lost
/// connection instead of failing silently.
#[test]
fn test_debug_connection_lost() {
    let mut app = connection_lost_app();
    app.run_debug("");
    let last = app
        .sessions
        .current
        .transcript
        .last()
        .expect("a line was pushed");
    match last {
        TranscriptLine::System(text) => {
            assert!(
                text.contains("debug: connection lost"),
                "expected connection-lost, got {text}"
            );
        }
        other => panic!("expected a system line, got {other:?}"),
    }
}
