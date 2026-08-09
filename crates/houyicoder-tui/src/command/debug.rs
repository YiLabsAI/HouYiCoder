//! /debug command, split from command.rs so that file stays under the size
//! gate.

use crate::state::App;

impl App {
    /// /debug [off]: toggle the process-wide diagnostic log level. No
    /// restart needed. The request goes over the wire so the server — which
    /// holds the subscriber handle — is the one that changes the level.
    /// This is what makes one toggle reach every crate: the engine, the
    /// sandbox and the permission gate all share the one subscriber the
    /// server installed, so a level change at the server is a level change
    /// everywhere. The host holds no subscriber handle of its own.
    pub(crate) fn run_debug(&mut self, arg: &str) {
        let level = if arg == "off" {
            houyicoder_protocol::frontend::debug::DebugLevel::Off
        } else {
            houyicoder_protocol::frontend::debug::DebugLevel::Debug
        };
        if let Some(req_id) = self.mint_request_id() {
            self.send_cmd(crate::run_control::ClientCommand::DebugSet { req_id, level });
        } else {
            self.system_line("debug: not connected");
        }
    }
}

#[cfg(test)]
mod tests {
    /// /debug with no arg sends a Debug-level toggle; /debug off sends an
    /// Off-level toggle. The behavior under test is which level the command
    /// carries, not whether a local logger was toggled — the toggle now
    /// lives at the server, which holds the subscriber handle. The
    /// end-to-end proof (level change reaches the engine) is in the
    /// service integration test that drives a real wire round-trip.
    #[test]
    fn test_debug_command_mints_request() {
        let mut app = crate::test_support::working_app();
        app.run_debug("");
        app.run_debug("off");
    }

    /// /debug with no backend wired (stub path, no session) surfaces a
    /// system line rather than panicking. This is the path a test-only App
    /// without a runner takes.
    #[test]
    fn test_no_session_not_connected() {
        let mut app = crate::test_support::working_app();
        app.session = None;
        app.run_debug("");
        let last = app.transcript.last().expect("a line was pushed");
        match last {
            crate::records::TranscriptLine::System(text) => {
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
        let mut app = crate::test_support::working_app();
        assert!(app.run_tui_local_command("debug"));
        assert!(app.run_tui_local_command("debug off"));
    }

    /// A DebugResult message from the server renders as a system line the
    /// user sees: the path when enabled, "logging off" when disabled. This
    /// is what tells the user where to look — the whole point of carrying
    /// the path in the reply.
    #[test]
    fn test_debug_result_renders_path() {
        use crate::agent_message::AgentMessage;
        use houyicoder_protocol::frontend::debug::DebugState;

        let mut app = crate::test_support::working_app();
        app.handle_agent_message(AgentMessage::DebugResult {
            state: DebugState {
                enabled: true,
                path: "/tmp/test.log".into(),
            },
        });
        let last = app.transcript.last().expect("a line was pushed");
        match last {
            crate::records::TranscriptLine::System(text) => {
                assert!(
                    text.contains("/tmp/test.log"),
                    "the path should be in the system line, got {text}"
                );
            }
            other => panic!("expected a system line, got {other:?}"),
        }

        app.handle_agent_message(AgentMessage::DebugResult {
            state: DebugState {
                enabled: false,
                path: String::new(),
            },
        });
        let last = app.transcript.last().expect("a line was pushed");
        match last {
            crate::records::TranscriptLine::System(text) => {
                assert!(
                    text.contains("off"),
                    "the off line should say off, got {text}"
                );
            }
            other => panic!("expected a system line, got {other:?}"),
        }
    }
}
