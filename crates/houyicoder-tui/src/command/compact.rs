//! The /compact command domain: guards against an in-flight run, mints a
//! request id, surfaces a "compacting" line, and sends the CompactQuery. The
//! server-side compaction fires PreCompact hooks, folds older events into a
//! summary, persists a CheckpointManifest, fires PostCompact, and replies
//! with the outcome.

use crate::state::App;

impl App {
    /// Handle the /compact slash command: refuse while a run is in flight so
    /// compaction never races the live turn's served view (compacting mid-run
    /// would corrupt the window the run is reading), then send the CompactQuery.
    /// The served view picks up the manifest on the next turn, so /compact does
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
        let Some(req_id) = self.mint_request_id() else {
            self.system_line("compact: no server connected");
            return;
        };
        self.system_line("compact: compacting...".to_string());
        self.send_cmd(crate::run_control::ClientCommand::CompactQuery { req_id });
    }
}
