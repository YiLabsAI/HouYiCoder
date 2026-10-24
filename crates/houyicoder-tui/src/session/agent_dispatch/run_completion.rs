//! Finalizes completed runs by clearing transient state, rebuilding the
//! transcript, and recording the outcome. The final status controls whether
//! queued input may drain.

use houyicoder_protocol::frontend::run::{RunOutcome, RunResult};

use crate::records::TranscriptLine;
use crate::transcript::FrontendRow;
use crate::turn_driver::should_preserve_interrupted_turn;

impl super::App {
    /// Finalize a run. The error side is a display string, not the protocol
    /// error type: three sources settle a run (a server-classified run
    /// failure, a per-request error routed to the active run, and driver
    /// death), and all the presentation needs is the message.
    pub(crate) fn handle_run_completion(&mut self, result: Result<RunResult, String>) {
        let had_live_output = self.run_progress().is_some_and(|p| {
            !p.live_assistant_text.is_empty() || !p.live_reasoning_text.is_empty()
        });
        // Take the ActiveRun out by value. The transition to Idle is the
        // effect the settle needs, and the run's wall start is read here
        // before the ActiveRun drops. Dropping it releases the streaming
        // progress, so the preview and tool-runtime state need no manual
        // clear here.
        let active = self.sessions.current.run.finish();
        self.sessions.current.clear_prompt();
        self.rebuild_transcript();
        self.debug_render_done(self.sessions.current.transcript.frames());
        let was_final = match result {
            Ok(run) => {
                // Compute by reference before the match moves run.outcome.
                let final_outcome = matches!(&run.outcome, RunOutcome::FinalOutput { .. });
                match run.outcome {
                    RunOutcome::FinalOutput { .. } => {
                        self.sessions.current.run_metrics.cumulative_tokens +=
                            run.usage.total_tokens as u64;
                        self.sessions.current.run_metrics.cumulative_steps += run.turns;
                        if self
                            .sessions
                            .current
                            .run_metrics
                            .session_started_at
                            .is_none()
                        {
                            self.sessions.current.run_metrics.session_started_at =
                                active.as_ref().map(|r| r.started_at);
                        }
                    }
                    RunOutcome::Handoff { agent } => {
                        self.system_line(format!("handoff to {}", agent));
                    }
                    RunOutcome::Interrupted { reason } => {
                        // Restore the submission only when interruption arrived
                        // before real output, preserving any active draft.
                        tracing::debug!(reason, "interrupted");
                        let restored = match self.sessions.current.last_run_input.take() {
                            Some(text)
                                if !had_live_output
                                    && !should_preserve_interrupted_turn(
                                        self.sessions.current.transcript.frames(),
                                    )
                                    && self.ui.input.editor.is_empty() =>
                            {
                                // Remove the empty submission from transcript
                                // and history before restoring it for editing.
                                self.rewind_to_last_user_input();
                                self.ui.input.history.remove_last();
                                self.ui.input.editor.set(text);
                                true
                            }
                            _ => false,
                        };
                        if restored {
                            self.raise_frontend_row(FrontendRow::InputRestored);
                        }
                        self.push_transcript_line(TranscriptLine::Interrupted);
                    }
                    RunOutcome::VerifyFailed { summary } => {
                        // Failed verification remains resumable and does not drain queued input.
                        self.system_line(format!("verify failed: {summary}"));
                    }
                    RunOutcome::MaxTurnsReached { turns } => {
                        // Reaching the turn limit is resumable rather than a crash.
                        self.system_line(format!(
                            "reached max turns limit after {turns} turns — resume to continue"
                        ));
                    }
                    // Unknown outcomes need no additional presentation.
                    _ => {}
                }
                final_outcome
            }
            Err(message) => {
                self.system_line(format!("agent error: {message}"));
                false
            }
        };
        if !self.agent_busy() {
            self.sessions.current.last_run_input = None;
        }
        // Only final completion may advance queued input. Other outcomes
        // park pending messages for explicit user action.
        self.sessions.current.last_run_final = was_final;
        if !was_final {
            self.ui.input.queue.demote_to_parked();
        }
    }
}

#[cfg(test)]
mod tests;
