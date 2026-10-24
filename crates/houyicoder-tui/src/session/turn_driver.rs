//! User turn execution and inbound message polling.
//!
//! Starts, aborts, and rewinds user turns against the active session
//! connection, and polls inbound frames and events in bounded batches so
//! terminal input stays responsive during high-rate streaming.

use std::env::var;
use std::time::Instant;

use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::SessionUpdate;

use crate::agent_message::{ClientCommand, ServerEvent, SessionMessage};
use crate::input::queue::InputItem;
use crate::records::TranscriptLine;
use crate::scroll::RowRelease;
use crate::session::PollOutcome;
use crate::state::App;
use crate::state::enums::LiveBlock;
use crate::transcript::frame_payload::chunk_text;
use crate::transcript::{FrontendRow, SequencedFrame, TranscriptFrame, is_user_frame};

const MAX_REBUILD_FRAMES: usize = 500;
const PREPEND_BATCH: usize = 100;
const MAX_AGENT_MESSAGES_PER_POLL: usize = 4096;

/// How many messages a poll may take past the batch cap when the cap lands
/// on a user message. The host sends the delivery mark right after the
/// message it belongs to; a rebuild that saw the message without the mark
/// would read it as opening a turn of its own and close the running turn
/// early. The mark is the next frame in the channel, so a few extra polls
/// rejoin the pair; an idle channel ends the lookahead at once.
const DELIVERY_MARK_LOOKAHEAD: usize = 4;

mod history_read;
mod startup;
mod transcript_rebuild;

impl App {
    /// Start a user turn, steer input to the viewed child, or queue it while
    /// another turn is active. New turns render an immediate user echo after
    /// the enqueue is accepted. Returns false when the enqueue was refused.
    pub fn spawn_run(&mut self, input: String) -> bool {
        // A viewed child receives input directly and shows an optimistic echo.
        let steer = self
            .ui
            .teammate_view
            .as_ref()
            .filter(|_| !input.is_empty())
            .map(|v| {
                let completed = self
                    .sessions
                    .current
                    .capabilities
                    .fleet
                    .entries
                    .iter()
                    .find(|e| e.agent_id == v.child_sid)
                    .map(|e| e.completed.is_some())
                    .unwrap_or(true);
                (v.child_sid.clone(), completed)
            });
        if let Some((child_sid, completed)) = steer {
            if completed {
                // A completed child cannot accept input; notify from the parent
                // transcript so a child refetch cannot hide the message.
                self.exit_teammate_view();
                self.system_line("this child has finished — start a new task or /agents to review");
                return true;
            }
            // The echo follows the enqueue: a refused injection must not leave a
            // ghost copy in the child transcript.
            if let Err(e) = self.enqueue(ClientCommand::InjectToChild {
                child_sid,
                text: input.clone(),
            }) {
                self.system_line(Self::enqueue_failure_line("child", e));
                return false;
            }
            if let Some(view) = self.ui.teammate_view.as_mut() {
                view.transcript.push(TranscriptLine::User(input.clone()));
                view.pending_echo = Some(input);
                // The child's own scroll follows its tail so the echo lands in
                // view; the parent's scroll keeps its position untouched.
                view.scroll.follow_tail();
            }
            // Invalidate cached rows after the optimistic echo.
            self.bump_transcript_version();
            return true;
        }
        // Active or waiting runs park new input locally. A waiting run
        // (approval card up) does not accept a second submit — the card
        // is the gate, not agent_busy.
        if self.sessions.current.run.is_active() {
            let parked = InputItem::ParkedMessage(input.into());
            self.ui.input.queue.push(parked);
            self.promote_next_pending();
            return true;
        }
        let Some(s) = self.sessions.current.connection.as_ref() else {
            return false;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("run: request ids exhausted");
            return false;
        };
        let session_id = self.sessions.current.identity.session_id.clone();
        let content = vec![ContentBlock::Text {
            text: input.clone(),
        }];
        let disabled_skills = self.sessions.current.capabilities.skills.disabled.clone();
        // Send before touching any run state: a dead driver must not leave a
        // fake running turn behind.
        if let Err(e) = self.enqueue(ClientCommand::SendMessage {
            req_id,
            session_id,
            content,
            disabled_skills,
        }) {
            self.system_line(Self::enqueue_failure_line("run", e));
            return false;
        }
        // Only errors matching this request terminate the active run.
        self.sessions.current.run.start(req_id, Instant::now());
        self.sessions.current.todos.set_replaying_history(false);
        // Preserve the submitted input in case interruption restores the turn.
        self.sessions.current.last_run_input = Some(input.clone());
        // A fresh submission answers the previous turn's interruption, so its
        // notice and restore line leave the log before this turn's lines land:
        // both are rows the frontend raised, and a rebuild renders them again
        // from the log they would still sit in.
        let transcript = &mut self.sessions.current.transcript;
        transcript.with_frames_mut(clear_interruption_markers);
        self.rebuild_transcript();
        self.push_transcript_line(TranscriptLine::User(input));
        self.ui.displayed_tokens.set(0);
        // start() just built fresh progress, so these preview resets touch
        // default values; routed through the run for the same reason the
        // resume path clears: a future caller that reuses progress keeps
        // the reset instead of carrying stale state into the new turn.
        if let Some(p) = self.run_progress_mut() {
            p.last_delta_at = None;
            p.thinking_started_at = None;
            p.live_block = LiveBlock::None;
        }
        true
    }

    /// Apply all available agent messages and return whether state changed.
    /// Consecutive frames rebuild the transcript as one batch;
    /// other messages first flush preceding frames to preserve order.
    pub fn poll_agent(&mut self) -> bool {
        let mut applied = false;
        let mut batch: Vec<SequencedFrame> = Vec::new();
        let mut remaining = MAX_AGENT_MESSAGES_PER_POLL;
        let mut lookahead_used = false;
        while remaining > 0 {
            remaining -= 1;
            // Poll one owned message off the session so the session borrow
            // ends before the mutable dispatch below. The batch cap returns
            // control to terminal input even when producers remain saturated.
            let outcome = self.sessions.current.connection.as_mut().map(|s| s.poll());
            match outcome {
                Some(PollOutcome::Message(SessionMessage::Event(ServerEvent::Frame(frame)))) => {
                    batch.push(frame);
                    applied = true;
                }
                Some(PollOutcome::Message(m)) => {
                    self.apply_frames(batch.drain(..));
                    self.handle_agent_message(m);
                    applied = true;
                }
                Some(PollOutcome::Idle) | None => break,
                Some(PollOutcome::Closed) => {
                    // The driver ended without a death announcement (task
                    // panic or abort): the connection is lost even though no
                    // ConnectionLost message will arrive. The shared loss
                    // settlement runs the full cleanup only on the first
                    // observation; a later Closed (after an announced death
                    // already settled the loss) changes nothing and stays
                    // quiet — poll_agent reflects that by not re-marking
                    // dirty.
                    self.apply_frames(batch.drain(..));
                    if self.apply_connection_loss("connection driver stopped".into(), Vec::new()) {
                        applied = true;
                    }
                    break;
                }
            }
            // The cap can land between a user message and the delivery mark
            // the host sends beside it. Grant one small lookahead so both
            // frames fold in one rebuild instead of splitting across two.
            let batch_ends_on_user = batch.last().is_some_and(|sf| is_user_frame(sf.frame()));
            if remaining == 0 && !lookahead_used && batch_ends_on_user {
                lookahead_used = true;
                remaining = DELIVERY_MARK_LOOKAHEAD;
            }
        }
        self.apply_frames(batch);
        // Resolve a bounded number of resume rows per poll.
        if self.ui.navigation.resume_picker.open
            && let Some(catalog) = self.sessions.catalog.as_ref()
        {
            self.ui
                .navigation
                .resume_picker
                .resolve_rows(catalog.as_ref(), 3);
        }
        // Refresh status while idle. Active runs and reverse requests retain
        // exclusive ownership of response frames.
        let poll_due = self.sessions.current.capabilities.status.poll_due();
        if !self.agent_busy() && !self.reverse_request_in_flight() && poll_due {
            self.sessions.current.capabilities.status.mark_poll_sent();
            if let Some(s) = self.sessions.current.connection.as_ref() {
                match s.next_request_id() {
                    Ok(req_id) => {
                        if s.enqueue(ClientCommand::StatusQuery { req_id }).is_ok() {
                            // Seed the mode cache once so the status label renders at
                            // startup. A refused enqueue stays quiet: the
                            // ConnectionLost event announces the loss.
                            let permission = &self.sessions.current.capabilities.permission;
                            if permission.mode_cache.is_none() {
                                match s.next_request_id() {
                                    Ok(req_id) => {
                                        let _ = s
                                            .enqueue(ClientCommand::PermissionModeQuery { req_id });
                                    }
                                    Err(_) => self.note_request_id_exhausted(),
                                }
                            }
                        }
                    }
                    Err(_) => self.note_request_id_exhausted(),
                }
            }
        }
        applied
    }

    /// Env-gated render diagnostic at each turn boundary. Reads the parent's
    /// own published total, not the active view's count: the frames and
    /// transcript lengths are the parent's, and a child view on screen would
    /// otherwise mix parent state with a child-derived top.
    pub(crate) fn debug_render_done<F: AsRef<TranscriptFrame>>(&self, frames: &[F]) {
        if var("HICODER_DEBUG_RENDER").is_err() {
            return;
        }
        let total = self.whole_scroll().total.get();
        tracing::warn!(
            "[done] frames={} transcript={} cap={} total={} follow={} top={} approval={} busy={}",
            frames.len(),
            self.sessions.current.transcript.len(),
            self.whole_scroll().cap.get(),
            total,
            self.whole_scroll().is_following_tail(),
            self.whole_scroll().top_offset(),
            self.approval().is_some(),
            self.agent_busy(),
        );
    }

    /// Abort the active run. The driver propagates cancellation through the
    /// execution pipeline and records the request for auditing.
    pub fn abort_run(&mut self) {
        // Cancellation becomes visible only after the driver accepted it: a
        // refused abort would otherwise wait forever for a completion that
        // never comes.
        if let Err(e) = self.enqueue(ClientCommand::AbortRun {
            session_id: self.sessions.current.identity.session_id.clone(),
        }) {
            self.system_line(Self::enqueue_failure_line("run", e));
            return;
        }
        self.sessions.current.run.begin_cancel();
    }

    /// Remove the latest submitted turn from the frame log and rebuild the
    /// transcript. Used when interruption arrives before substantive output so
    /// the user can edit and resend the restored input. The rows the frontend
    /// raised during that turn go with it: they describe work now discarded.
    pub fn rewind_to_last_user_input(&mut self) {
        let transcript = &self.sessions.current.transcript;
        let Some(start) = transcript.frames().iter().rposition(|sf| {
            matches!(
                sf.as_ref(),
                TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_))
            )
        }) else {
            return;
        };
        let anchor = self.whole_scroll().anchor().cloned();
        let lost = anchor.is_some_and(|anchor| {
            transcript.frames()[start..]
                .iter()
                .any(|sf| anchor.row.names(sf.as_ref(), sf.event_id.as_ref()))
        });
        let transcript = &mut self.sessions.current.transcript;
        transcript.with_frames_mut(|log| log.truncate(start));
        self.sessions.current.todos.set_replaying_history(true);
        if lost {
            self.release_parked_row(RowRelease::RowLeftHistory);
        }
        self.rebuild_transcript();
    }
}

/// Clear the previous run's interruption markers: the Interrupted notice and
/// the restore line that can precede it.
fn clear_interruption_markers(frames: &mut Vec<SequencedFrame>) {
    frames.retain(|sf| {
        !matches!(
            sf.as_ref(),
            TranscriptFrame::Frontend(FrontendRow::Interrupted | FrontendRow::InputRestored)
        )
    });
}

/// Whether an interrupted turn must remain submitted. Assistant output, tool
/// calls, and reasoning preserve the turn; a user-only turn may be restored.
/// Missing user context preserves the turn conservatively.
pub(crate) fn should_preserve_interrupted_turn<F: AsRef<TranscriptFrame>>(frames: &[F]) -> bool {
    let Some(start) = frames.iter().rposition(|sf| {
        matches!(
            sf.as_ref(),
            TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_))
        )
    }) else {
        return true;
    };
    frames[start + 1..].iter().any(|sf| match sf.as_ref() {
        TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(chunk)) => {
            !chunk_text(chunk).is_empty()
        }
        TranscriptFrame::Session(SessionUpdate::ToolCall(_)) => true,
        TranscriptFrame::Session(SessionUpdate::AgentThoughtChunk(_)) => true,
        _ => false,
    })
}

#[cfg(test)]
pub(crate) mod tests;
