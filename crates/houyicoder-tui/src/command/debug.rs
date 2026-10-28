//! /debug command: set the engine-side debug level through the server.

use houyicoder_protocol::frontend::debug::{DebugLevel, DebugState};

use crate::agent_message::ClientCommand;
use crate::state::App;

impl App {
    /// /debug [off]: toggle the process-wide diagnostic log level. No
    /// restart needed. The request goes to the server — which holds the
    /// subscriber handle — so the server is the one that changes the level.
    /// This is what makes one toggle reach every crate: the engine, the
    /// sandbox and the permission gate all share the one subscriber the
    /// server installed, so a level change at the server is a level change
    /// everywhere. The host holds no subscriber handle of its own.
    pub(crate) fn run_debug(&mut self, arg: &str) {
        let level = if arg == "off" {
            DebugLevel::Off
        } else {
            DebugLevel::Debug
        };
        let Some(s) = self.sessions.current.connection.as_ref() else {
            self.system_line("debug: not connected");
            return;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("debug: request ids exhausted");
            return;
        };
        if let Err(e) = self.enqueue(ClientCommand::DebugSet { req_id, level }) {
            self.system_line(Self::enqueue_failure_line("debug", e));
        }
    }

    /// Report the level the host now records at, and where it writes. The
    /// path travels with the answer because the user has to open that file
    /// to read what the toggle turned on.
    pub(crate) fn apply_debug_state(&mut self, state: DebugState) {
        if state.enabled {
            self.system_line(format!("debug: logging to {}", state.path));
        } else {
            self.system_line("debug: logging off");
        }
    }
}

#[cfg(test)]
mod tests;
