//! The /debug request handler, split from server.rs so that file stays
//! under the size gate. Same pattern as the other child modules under
//! server/.

use houyicoder_protocol::envelope::ResponsePayload;
use houyicoder_protocol::frontend::debug::DebugLevel;
use houyicoder_protocol::wire::{WireError, WireErrorKind};
use tracing_subscriber::filter::LevelFilter;

use super::Server;

impl Server {
    /// The /debug reply: set the level when a handle is present, then report
    /// the resulting state. Split out of the dispatch match so the
    /// set-then-report logic is unit-testable without driving the wire.
    /// Returns an error when no sink was installed, so the host can surface
    /// that the diagnostic sink is absent rather than silently succeeding.
    pub(super) fn debug_response(&self, level: DebugLevel) -> ResponsePayload {
        let Some(h) = &self.diagnostics else {
            return ResponsePayload::Error(WireError::new(
                WireErrorKind::InvalidRequest,
                "no diagnostic sink is installed in this process",
                false,
            ));
        };
        let filter = match level {
            DebugLevel::Off => LevelFilter::OFF,
            DebugLevel::Debug => LevelFilter::DEBUG,
        };
        if let Err(e) = h.set_level(filter) {
            return ResponsePayload::Error(WireError::new(
                WireErrorKind::Internal,
                format!("could not change the diagnostic level: {e}"),
                false,
            ));
        }
        ResponsePayload::Debug(houyicoder_protocol::frontend::debug::DebugState {
            enabled: filter != LevelFilter::OFF,
            path: h
                .path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        })
    }
}

#[cfg(test)]
#[path = "debug_dispatch_tests.rs"]
mod debug_dispatch_tests;
