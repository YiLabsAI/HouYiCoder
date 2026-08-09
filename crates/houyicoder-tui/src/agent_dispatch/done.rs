//! Run-end dispatch: clear run-lifecycle state, rebuild the transcript from
//! the frame log, surface the outcome, and return was_final so the event
//! loop can gate the queued-message drain (a user interrupt must not
//! auto-send the next queued message; a deferred resume swap drains any-end).

use crate::records::TranscriptLine;
use houyicoder_protocol::frontend::run::{RunError, RunOutcome, RunResult};

impl super::App {
    pub(super) fn handle_run_done(&mut self, result: Result<RunResult, RunError>) {
        self.agent_busy = false;
        self.live_active = false;
        self.live_assistant_text.clear();
        self.live_reasoning_text.clear();
        self.live_block = crate::state::enums::LiveBlock::None;
        self.thinking_started_at = None;
        self.running_tools.clear();
        self.bash_progress.clear();
        self.pending_permission_req_id.set(None);
        // Any run end clears the cancelling flag: an Interrupted
        // outcome is the expected end of a cancel; any other outcome
        // means the run finished on its own (a stale cancel is moot).
        self.cancelling = false;
        let elapsed_secs = self.run_started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        self.run_started = None;
        // Rebuild the transcript from App's own frame log,
        // interleaving TUI-only lines at their correct positions.
        self.rebuild_transcript();
        self.debug_render_done(&self.frames);
        // was_final: whether the run ended FinalOutput. Returned to the
        // caller (the event loop) so it gates the queued-message drain on
        // it -- a user interrupt should NOT auto-send the next queued
        // message (intentional), but a deferred resume swap still drains on
        // any run end. The wire carries no Interruption outcome (a mid-turn
        // permission ask is a reverse request, not a run end).
        let was_final = match result {
            Ok(run) => {
                self.status.tokens = run.usage.total_tokens as u64;
                // Compute by reference before the match moves run.outcome.
                let final_outcome = matches!(&run.outcome, RunOutcome::FinalOutput { .. });
                match run.outcome {
                    RunOutcome::FinalOutput { .. } => {
                        self.cumulative_tokens += run.usage.total_tokens as u64;
                        self.cumulative_steps += run.turns;
                        if self.session_started_at.is_none() {
                            self.session_started_at = self.run_started;
                        }
                        let reasoning: Option<String> =
                            crate::transcript::turn_reasoning(&self.frames);
                        let tool_summary: Option<String> =
                            crate::transcript::turn_tool_summary(&self.frames);
                        self.turn_seq = self.turn_seq.saturating_add(1);
                        let turn_id = self.turn_seq.to_string();
                        self.push_transcript_line(TranscriptLine::ThoughtFor {
                            secs: elapsed_secs as u32,
                            reasoning,
                            tool_summary,
                            turn_id,
                        });
                    }
                    RunOutcome::Handoff { agent } => {
                        self.system_line(format!("handoff to {}", agent));
                    }
                    RunOutcome::Interrupted { reason } => {
                        // External abort: partial text flushed, tool calls
                        // got a synthetic interrupted result so the session
                        // stays resumable. If the model produced no real
                        // content after the last user input, restore the
                        // input so the user can edit and resend.
                        tracing::debug!(reason, "interrupted");
                        let restored = match self.last_run_input.take() {
                            Some(text)
                                if !super::super::run_produced_real_content(&self.frames) =>
                            {
                                // Rewind the frame log past the user echo and
                                // any partial turn content so the transcript
                                // drops the user line (the submit is undone),
                                // then restore the input for edit + resend.
                                self.rewind_to_last_user_input();
                                self.input.set(text);
                                true
                            }
                            _ => false,
                        };
                        if restored {
                            self.system_line("input restored");
                        }
                        self.push_transcript_line(crate::records::TranscriptLine::Interrupted);
                    }
                    RunOutcome::VerifyFailed { summary } => {
                        // Post-run verify gate rejected the output. Surface
                        // the summary as a dim system line; do not drain so
                        // the caller can re-prompt.
                        self.system_line(format!("verify failed: {summary}"));
                    }
                    RunOutcome::MaxTurnsReached { turns } => {
                        // Graceful max-turns backstop: resumable, not a
                        // crash. Surface the resume hint with the turn count.
                        self.system_line(format!(
                            "reached max turns limit after {turns} turns — resume to continue"
                        ));
                    }
                    // A future wire outcome the frontend does not model yet.
                    // The transcript is already rebuilt from the event log
                    // above; surface nothing extra.
                    _ => {}
                }
                // The queued-message drain moved to the event loop's idle
                // guard (app.rs): resume target drains on any run end, a
                // queued message only when was_final (a user interrupt must
                // not auto-send the next queued message).
                final_outcome
            }
            Err(e) => {
                self.system_line(format!("agent error: {}", e.message));
                false
            }
        };
        if !self.agent_busy {
            self.last_run_input = None;
        }
        // Record was_final for the event loop's idle drain: a clean
        // FinalOutput end auto-sends the next queued item (drain FIFO); an
        // interrupt/error parks it for the user to pop to the input box via
        // Esc + edit before re-sending (a redirect on interrupt should not
        // auto-fire the pending input). A non-final end orphans the server
        // buffer, so demote any Message still in the pending copy here. Gated on
        // was_final so a new outcome added above cannot bypass it.
        self.status.last_run_final = was_final;
        if !was_final {
            self.demote_pending_to_parked();
        }
    }
}
