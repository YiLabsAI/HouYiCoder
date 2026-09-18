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
        self.live_reasoning_text.clear();
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
                        let reasoning: Option<String> = turn_reasoning(&self.frames);
                        let tool_summary: Option<String> = turn_tool_summary(&self.frames);
                        self.turn_seq = self.turn_seq.saturating_add(1);
                        let turn_id = self.turn_seq.to_string();
                        self.mint_thought_or_repair(
                            turn_id,
                            elapsed_secs as u32,
                            reasoning,
                            tool_summary,
                        );
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

    /// Record the just-completed turn's thought row. A resumed session's
    /// fresh fold leaves an 'r'-prefixed ThoughtFor for the in-progress turn
    /// (no later user closed it), so this turns it into a live row with the
    /// real seconds and this run's turn id rather than pushing a second row
    /// for the same turn. A normal live turn has no such row and pushes.
    fn mint_thought_or_repair(
        &mut self,
        turn_id: String,
        secs: u32,
        reasoning: Option<String>,
        tool_summary: Option<String>,
    ) {
        let last_user = self
            .transcript
            .iter()
            .rposition(|l| matches!(l, TranscriptLine::User(_)));
        let seg_start = last_user.map(|i| i + 1).unwrap_or(0);
        let found = self.transcript[seg_start..].iter().rposition(
            |l| matches!(l, TranscriptLine::ThoughtFor { turn_id: t, .. } if t.starts_with('r')),
        );
        if let Some(rel) = found {
            let idx = seg_start + rel;
            if let TranscriptLine::ThoughtFor {
                secs: s,
                turn_id: t,
                reasoning: r,
                tool_summary: ts,
                ..
            } = &mut self.transcript[idx]
            {
                *s = secs;
                *t = turn_id;
                *r = reasoning;
                *ts = tool_summary;
                self.bump_transcript_version();
            }
            return;
        }
        self.push_transcript_line(TranscriptLine::ThoughtFor {
            secs,
            reasoning,
            tool_summary,
            turn_id,
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::records::TranscriptLine;

    /// A resumed session's fresh fold leaves the in-progress turn's 'r'
    /// ThoughtFor with no later user; completing that run repairs it in place
    /// (real seconds + this run's turn id) instead of pushing a second row.
    #[test]
    fn test_mint_repairs_resumed_turn() {
        let mut app = crate::composition::app();
        app.transcript.push(TranscriptLine::User("go".into()));
        app.transcript.push(TranscriptLine::ThoughtFor {
            secs: 0,
            reasoning: Some("folded".into()),
            tool_summary: None,
            turn_id: "r1".into(),
        });
        app.mint_thought_or_repair(
            "3".into(),
            12,
            Some("live".into()),
            Some("ran 1 tool".into()),
        );
        let thoughts: Vec<String> = app
            .transcript
            .iter()
            .filter_map(|l| match l {
                TranscriptLine::ThoughtFor {
                    secs,
                    turn_id,
                    reasoning,
                    ..
                } => Some(format!("{secs}:{turn_id}:") + &reasoning.clone().unwrap_or_default()),
                _ => None,
            })
            .collect();
        assert_eq!(
            thoughts,
            vec!["12:3:live".to_string()],
            "the resumed turn is repaired in place: {thoughts:?}"
        );
    }

    /// A normal live turn has no 'r' row to repair: push a fresh ThoughtFor.
    #[test]
    fn test_mint_pushes_fresh_turn() {
        let mut app = crate::composition::app();
        app.transcript.push(TranscriptLine::User("go".into()));
        app.mint_thought_or_repair("1".into(), 5, None, None);
        let thoughts = app
            .transcript
            .iter()
            .filter(|l| matches!(l, TranscriptLine::ThoughtFor { .. }))
            .count();
        assert_eq!(thoughts, 1, "a fresh live turn pushes one row");
    }
}
