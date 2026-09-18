//! Finalizes completed runs by clearing transient state, rebuilding the
//! transcript, and recording the outcome. The final status controls whether
//! queued input may drain.

use houyicoder_protocol::frontend::run::{RunOutcome, RunResult};

use super::super::should_preserve_interrupted_turn;
use crate::records::TranscriptLine;
use crate::state::enums::LiveBlock;
use crate::transcript::{turn_reasoning, turn_tool_summary};

impl super::App {
    /// Finalize a run. The error side is a display string, not the protocol
    /// error type: three sources settle a run (a server-classified run
    /// failure, a per-request error routed to the active run, and driver
    /// death), and all the presentation needs is the message.
    pub(super) fn handle_run_completion(&mut self, result: Result<RunResult, String>) {
        let had_live_output =
            !self.live_assistant_text.is_empty() || !self.live_reasoning_text.is_empty();
        // finish() returns the ActiveRun by value and transitions to Idle;
        // capture it so the elapsed computation can read started_at before
        // the run state is gone.
        let finished = self.run_state.finish();
        let elapsed_secs = finished
            .as_ref()
            .map(|r| r.started_at.elapsed().as_secs())
            .unwrap_or(0);
        self.live_active = false;
        self.live_assistant_text.clear();
        // Capture the live reasoning stream before clearing: the authoritative
        // Reasoning event is persisted and forwarded as an AgentThoughtChunk
        // frame, but that frame may land after the run-completion message, so
        // turn_reasoning(&frames) can return None at Done. The live stream
        // holds this turn's reasoning (the spinner showed Thinking), so it
        // backs the ThoughtFor row when the frame has not arrived yet.
        let live_reasoning = std::mem::take(&mut self.live_reasoning_text);
        self.live_block = LiveBlock::None;
        self.thinking_started_at = None;
        self.running_tools.clear();
        self.bash_progress.clear();
        self.pending_permission_req_id.set(None);
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
                            self.session_started_at = self.run_started();
                        }
                        let reasoning: Option<String> = turn_reasoning(&self.frames)
                            .or_else(|| (!live_reasoning.is_empty()).then_some(live_reasoning));
                        let tool_summary: Option<String> = turn_tool_summary(&self.frames);
                        // A turn with neither reasoning nor a tool call is a
                        // plain text reply — a ThoughtFor row there renders
                        // "Thought for Ns" with no expandable content, which
                        // reads as a broken affordance. Skip it unless this
                        // turn carried reasoning or a tool summary.
                        if reasoning.is_some() || tool_summary.is_some() {
                            self.turn_seq = self.turn_seq.saturating_add(1);
                            let turn_id = self.turn_seq.to_string();
                            self.push_transcript_line(TranscriptLine::ThoughtFor {
                                secs: elapsed_secs as u32,
                                reasoning,
                                tool_summary,
                                turn_id,
                            });
                        }
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
                                if !had_live_output
                                    && !should_preserve_interrupted_turn(&self.frames)
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
            Err(message) => {
                self.system_line(format!("agent error: {message}"));
                false
            }
        };
        if !self.agent_busy() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A turn that produced neither reasoning nor a tool call is a plain
    /// text reply; it must not push a ThoughtFor row, whose "Thought for Ns"
    /// with no expandable content reads as a broken affordance.
    #[test]
    fn test_plain_reply_skips_thought() {
        use houyicoder_protocol::frontend::run::{RunOutcome, RunResult, StopReason};
        use houyicoder_protocol::llm::Usage;
        let mut app = crate::composition::app();
        app.handle_run_completion(Ok(RunResult {
            outcome: RunOutcome::FinalOutput {
                content: vec![houyicoder_protocol::frontend::run::ContentBlock::Text {
                    text: "hi".into(),
                }],
            },
            usage: Usage::default(),
            turns: 1,
            stop_reason: StopReason::EndTurn,
        }));
        let has_thought = app
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::ThoughtFor { .. }));
        assert!(!has_thought, "a plain reply must not push a ThoughtFor row");
    }

    /// A turn whose frames carry reasoning pushes a ThoughtFor row carrying
    /// that reasoning, so the row is expandable.
    #[test]
    fn test_reasoning_turn_pushes_thought() {
        use crate::transcript::TranscriptFrame;
        use houyicoder_protocol::frontend::ContentBlock;
        use houyicoder_protocol::frontend::run::{RunOutcome, RunResult, StopReason};
        use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};
        use houyicoder_protocol::llm::Usage;
        let mut app = crate::composition::app();
        app.frames
            .push(TranscriptFrame::Session(SessionUpdate::AgentThoughtChunk(
                ContentChunk::new(ContentBlock::Text {
                    text: "weighing the options".into(),
                }),
            )));
        app.handle_run_completion(Ok(RunResult {
            outcome: RunOutcome::FinalOutput {
                content: vec![ContentBlock::Text {
                    text: "answer".into(),
                }],
            },
            usage: Usage::default(),
            turns: 1,
            stop_reason: StopReason::EndTurn,
        }));
        let has_thought = app.transcript.iter().any(|l| {
            matches!(
                l,
                TranscriptLine::ThoughtFor { reasoning: Some(r), .. }
                    if r.contains("weighing the options")
            )
        });
        assert!(
            has_thought,
            "a turn with reasoning must push an expandable ThoughtFor row"
        );
    }
}
