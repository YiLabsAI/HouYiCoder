//! Starts manual compaction when the session is idle and connected.

use crate::agent_message::ClientCommand;
use crate::records::TranscriptLine;
use crate::state::App;
use crate::transcript::{FrontendRow, TranscriptFrame};

impl App {
    /// True while a manual /compact request is in flight and waiting for its
    /// reply.
    pub fn compacting(&self) -> bool {
        self.sessions
            .current
            .transcript
            .lines()
            .iter()
            .rev()
            .take(4)
            .any(|l| matches!(l, TranscriptLine::System(s) if s == "compact: compacting..."))
    }

    /// Remove the transient "compact: compacting..." notice once the compaction
    /// reply arrives so the transcript only keeps the final outcome.
    pub fn drop_compacting_notice(&mut self) {
        let at = self
            .sessions
            .current
            .transcript
            .frames()
            .iter()
            .rposition(|sf| {
                matches!(
                    sf.as_ref(),
                    TranscriptFrame::Frontend(FrontendRow::System(s)) if s == "compact: compacting..."
                )
            });
        if let Some(at) = at {
            self.sessions.current.transcript.with_frames_mut(|log| {
                log.remove(at);
            });
            self.rebuild_transcript();
        }
    }
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
        if self.shell_run_active() {
            self.system_line(
                "compact: a shell command is running; wait for it to finish (or Esc to stop it) before compacting",
            );
            return;
        }
        if self.agent_busy() || self.compacting() {
            self.system_line(
                "compact: a run is in flight; wait for it to finish (or Esc to abort) before compacting",
            );
            return;
        }
        let Some(s) = self.sessions.current.connection.as_ref() else {
            self.system_line("compact: not connected");
            return;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("compact: request ids exhausted");
            return;
        };
        // The in-progress line waits for the send: a dead driver must not
        // leave the user watching a compaction that never started.
        if let Err(e) = self.enqueue(ClientCommand::CompactQuery { req_id }) {
            self.system_line(Self::enqueue_failure_line("compact", e));
            return;
        }
        self.system_line("compact: compacting...".to_string());
    }
}
