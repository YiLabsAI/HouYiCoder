//! Finalizes completed runs by clearing transient state, rebuilding the
//! transcript, and recording the outcome. The final status controls whether
//! queued input may drain.

use houyicoder_protocol::frontend::run::{RunOutcome, RunResult};

use super::super::should_preserve_interrupted_turn;
use crate::records::TranscriptLine;
use crate::transcript::FrontendRow;

impl super::App {
    /// Finalize a run. The error side is a display string, not the protocol
    /// error type: three sources settle a run (a server-classified run
    /// failure, a per-request error routed to the active run, and driver
    /// death), and all the presentation needs is the message.
    pub(super) fn handle_run_completion(&mut self, result: Result<RunResult, String>) {
        let had_live_output = self.run_progress().is_some_and(|p| {
            !p.live_assistant_text.is_empty() || !p.live_reasoning_text.is_empty()
        });
        // The transition to Idle is the effect the settle needs; the ActiveRun
        // the call hands back has no reader once the run's own frames carry
        // what the turn did. Dropping it releases the streaming progress, so
        // the preview and tool-runtime state need no manual clear here.
        self.run_state.finish();
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
    /// text reply; the rebuild must not surface a summary row for it, whose
    /// affordance with no expandable content reads as broken.
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

    /// A turn whose frames carry reasoning surfaces a summary row carrying that
    /// reasoning, so the row is expandable. Deriving the row at the rebuild is
    /// what lets a turn replayed from the log show the row the live turn showed.
    #[test]
    fn test_reasoning_turn_keeps_thought() {
        use crate::transcript::TranscriptFrame;
        use houyicoder_protocol::frontend::ContentBlock;
        use houyicoder_protocol::frontend::run::{RunOutcome, RunResult, StopReason};
        use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};
        use houyicoder_protocol::llm::Usage;
        let mut app = crate::composition::app();
        app.frames
            .push(TranscriptFrame::Session(SessionUpdate::UserMessageChunk(
                ContentChunk::new(ContentBlock::Text { text: "go".into() }),
            )));
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
            "a turn with reasoning must surface its summary row"
        );
    }
}
