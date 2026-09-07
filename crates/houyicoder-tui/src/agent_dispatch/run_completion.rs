//! Finalizes completed runs by clearing transient state, rebuilding the
//! transcript, and recording the outcome. The final status controls whether
//! queued input may drain.

use houyicoder_protocol::frontend::run::{RunError, RunOutcome, RunResult};

use super::super::should_preserve_interrupted_turn;
use crate::records::TranscriptLine;
use crate::state::enums::LiveBlock;
use crate::transcript::{turn_reasoning, turn_tool_summary};

impl super::App {
    pub(super) fn handle_run_completion(&mut self, result: Result<RunResult, RunError>) {
        self.agent_busy = false;
        self.live_active = false;
        self.live_assistant_text.clear();
        self.live_reasoning_text.clear();
        self.live_block = LiveBlock::None;
        self.thinking_started_at = None;
        self.running_tools.clear();
        self.bash_progress.clear();
        self.pending_permission_req_id.set(None);
        // Every terminal outcome ends cancellation.
        self.cancelling = false;
        let elapsed_secs = self.run_started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        self.run_started = None;
        self.rebuild_transcript();
        self.debug_render_done(&self.frames);
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
                        let reasoning: Option<String> = turn_reasoning(&self.frames);
                        let tool_summary: Option<String> = turn_tool_summary(&self.frames);
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
                        // Restore the submission only when interruption arrived
                        // before real output, preserving any active draft.
                        tracing::debug!(reason, "interrupted");
                        let restored = match self.last_run_input.take() {
                            Some(text)
                                if !should_preserve_interrupted_turn(&self.frames)
                                    && self.input.is_empty() =>
                            {
                                // Remove the empty submission from transcript
                                // and history before restoring it for editing.
                                self.rewind_to_last_user_input();
                                self.history.remove_last();
                                self.input.set(text);
                                true
                            }
                            _ => false,
                        };
                        if restored {
                            self.system_line("input restored");
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
            Err(e) => {
                self.system_line(format!("agent error: {}", e.message));
                false
            }
        };
        if !self.agent_busy {
            self.last_run_input = None;
        }
        // Only final completion may advance queued input. Other outcomes
        // park pending messages for explicit user action.
        self.status.last_run_final = was_final;
        if !was_final {
            self.demote_pending_to_parked();
        }
    }
}
