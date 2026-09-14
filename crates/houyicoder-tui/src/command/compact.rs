//! Starts manual compaction when the session is idle and connected.

use crate::state::App;

impl App {
    /// Handle the /compact slash command: refuse while a run is in flight so
    /// compaction never races the live turn's assembled context (compacting mid-run
    /// would corrupt the window the run is reading), then send the CompactQuery.
    /// The assembled context picks up the manifest on the next turn, so /compact does
    /// not reduce the in-flight context immediately — a second /compact before
    /// the next turn sees the same content plus the first compact's output, so
    /// the "before" count grows. Push a "compacting..." system line so the user
    /// sees the operation is in flight (the outcome line lands when the reply
    /// arrives).
    pub(crate) fn run_compact(&mut self) {
        if self.agent_busy {
            self.system_line(
                "compact: a run is in flight; wait for it to finish (or Esc to abort) before compacting",
            );
            return;
        }
        let Some(req_id) = self.next_request_id() else {
            self.system_line("compact: not connected");
            return;
        };
        // The in-progress line waits for the send: a dead driver must not
        // leave the user watching a compaction that never started.
        if !self.send_cmd(crate::run_control::ClientCommand::CompactQuery { req_id }) {
            self.system_line("compact: connection lost");
            return;
        }
        self.system_line("compact: compacting...".to_string());
    }
}
