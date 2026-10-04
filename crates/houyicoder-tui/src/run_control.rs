//! Coordinates user turns between application state and the protocol client.
//! Commands travel through the client driver; returned frames form the durable
//! transcript, while streaming events update the live presentation.

use std::time::{Duration, Instant};

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::extension::ENTITLEMENT_TOOL;
use houyicoder_protocol::frontend::permission::{AskSource, PermissionMode};
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalRequest, ContentBlock};
use houyicoder_protocol::frontend::session_update::SessionUpdate;

use crate::pending_prompt::PendingPrompt;
use crate::pending_queue::PendingItem;
use crate::records::{Approval, AskQuestion, TranscriptLine};
use crate::session::{ConnectionStatus, EnqueueError, PollOutcome};
use crate::state::App;
use crate::state::enums::LiveBlock;
use crate::transcript::frame_payload::chunk_text;
use crate::transcript::{FrontendRow, SequencedFrame, TranscriptFrame, is_user_frame};

const MAX_REBUILD_FRAMES: usize = 500;
const PREPEND_BATCH: usize = 100;
const MAX_AGENT_MESSAGES_PER_POLL: usize = 4096;

/// How many messages a poll may take past the batch cap when the cap lands
/// on a user message. The projection writes the delivery mark beside the
/// message it belongs to; a rebuild that saw the message without the mark
/// would read it as opening a turn of its own and close the running turn
/// early. The mark is the next frame in the channel, so a few extra polls
/// rejoin the pair; an idle channel ends the lookahead at once.
const DELIVERY_MARK_LOOKAHEAD: usize = 4;

#[path = "run_control/history_read.rs"]
mod history_read;
#[path = "run_control/transcript_rebuild.rs"]
mod transcript_rebuild;

pub use crate::agent_message::ClientCommand;
use crate::agent_message::{ServerEvent, SessionMessage};

impl App {
    /// Enqueue a command over the session's command channel. Ok means the
    /// command entered the local connection queue, not that it reached the
    /// transport or server. NotConnected when no active session exists; Closed
    /// when the driver task is gone. In either Err case the command never
    /// left this process, so no reply will come and the caller must not
    /// leave state waiting on one. Request ids are issued by the connection
    /// before this call.
    pub fn enqueue(&self, cmd: ClientCommand) -> Result<(), EnqueueError> {
        self.session
            .as_ref()
            .ok_or(EnqueueError::NotConnected)
            .and_then(|s| s.enqueue(cmd))
    }

    /// The user-facing line for an enqueue failure on a user-initiated action.
    /// NotConnected and Closed stay distinct so the user sees the real cause
    /// rather than a generic loss message. Presentation lives on the App
    /// boundary, not on the connection error type.
    pub(crate) fn enqueue_failure_line(subject: &str, error: EnqueueError) -> String {
        match error {
            EnqueueError::NotConnected => format!("{subject}: not connected"),
            EnqueueError::Closed => format!("{subject}: connection lost"),
        }
    }

    /// Enqueue an auto-refresh query and settle its result without writing a
    /// transcript line. The driver's ConnectionLost event is the single
    /// visible connection-failure notice; a second line here would race with
    /// it. NotConnected means no session exists and there is nothing to
    /// refresh. Callers with pending flags clear them at the enqueue site on
    /// the Err path.
    pub(crate) fn enqueue_refresh(&mut self, command: ClientCommand) {
        if let Err(_error) = self.enqueue(command) {
            // Acknowledged, not dropped: the ConnectionLost event announces
            // the loss and sweeps state on the next poll.
        }
    }

    /// The lifecycle of the current connection. Disconnected is the absent
    /// session, so callers read one four-state view instead of folding an
    /// Option over a three-state enum.
    pub fn connection_status(&self) -> ConnectionStatus {
        self.session
            .as_ref()
            .map_or(ConnectionStatus::Disconnected, |s| s.status().clone())
    }

    /// Record a single request-id exhaustion notice for an auto path. The
    /// connection refuses further allocations until it ends, so each refresh
    /// round that observes the failure would otherwise either spam the line or
    /// lose it silently; the one-shot flag lets the first round announce and
    /// later rounds stay quiet.
    pub(crate) fn note_request_id_exhausted(&mut self) {
        if self
            .session
            .as_ref()
            .is_some_and(|s| s.take_exhaustion_notice())
        {
            self.system_line("request ids exhausted");
        }
    }

    /// Start a user turn, steer input to the viewed child, or queue it while
    /// another turn is active. New turns render an immediate user echo after
    /// the enqueue is accepted. Returns false when the enqueue was refused.
    pub fn spawn_run(&mut self, input: String) -> bool {
        // A viewed child receives input directly and shows an optimistic echo.
        let steer = self
            .teammate_view
            .as_ref()
            .filter(|_| !input.is_empty())
            .map(|v| {
                let completed = self
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
            if let Some(view) = self.teammate_view.as_mut() {
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
        if self.run_state.is_active() {
            self.pending
                .push(PendingItem::ParkedMessage(input.clone().into()));
            self.promote_next_pending();
            return true;
        }
        let Some(s) = self.session.as_ref() else {
            return false;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("run: request ids exhausted");
            return false;
        };
        let session_id = self.session_id.clone();
        let content = vec![ContentBlock::Text {
            text: input.clone(),
        }];
        let disabled_skills = self.skill_disabled.clone();
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
        self.run_state.start(req_id, Instant::now());
        self.todos.set_replaying_history(false);
        // Preserve the submitted input in case interruption restores the turn.
        self.last_run_input = Some(input.clone());
        // A fresh submission answers the previous turn's interruption, so its
        // notice and restore line leave the log before this turn's lines land:
        // both are rows the frontend raised, and a rebuild renders them again
        // from the log they would still sit in.
        self.transcript.with_frames_mut(clear_interruption_markers);
        self.rebuild_transcript();
        self.push_transcript_line(TranscriptLine::User(input));
        self.displayed_tokens.set(0);
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

    /// Consume the pending queue head in first-in, first-out order. Clean
    /// completion may start the next turn; other outcomes leave input parked.
    /// At most one parked message is promoted to the server queue. Returns
    /// false when no item is available or the enqueue was refused.
    pub fn drain_pending_head(&mut self) -> bool {
        let Some(item) = self.pending.first().cloned() else {
            return false;
        };
        match item {
            PendingItem::Command(text) => {
                self.pending.remove(0);
                self.run_slash_text(&text);
                true
            }
            PendingItem::Message(head) => {
                // Remove the stale server copy before starting a fresh run; a
                // refused removal keeps the head queued so the local copy and
                // the server mirror cannot diverge.
                let session_id = self.session_id.clone();
                if let Err(e) = self.enqueue(ClientCommand::QueueRemove {
                    session_id,
                    id: head.id,
                }) {
                    self.system_line(Self::enqueue_failure_line("queue", e));
                    return false;
                }
                self.pending.remove(0);
                let text = head.text.clone();
                if !self.spawn_run(head.text) {
                    // The mirror is gone but the run could not start: keep the
                    // user content parked at the head so nothing is lost.
                    self.pending
                        .insert(0, PendingItem::ParkedMessage(text.into()));
                    return false;
                }
                self.promote_next_pending();
                true
            }
            PendingItem::ParkedMessage(input) => {
                self.pending.remove(0);
                let text = input.text.clone();
                if !self.spawn_run(input.text) {
                    self.pending
                        .insert(0, PendingItem::ParkedMessage(text.into()));
                    return false;
                }
                self.promote_next_pending();
                true
            }
        }
    }

    /// Resolve the active approval, clear its interface state, and send the
    /// verdict as the matching reverse response. No-op when no request awaits
    /// a decision.
    pub fn resolve_current_approval(&mut self, decision: ApprovalDecision) {
        let req_id = match self.prompt.as_ref() {
            Some(PendingPrompt::Permission { req_id, .. }) => *req_id,
            _ => return,
        };
        // Enqueue first: the card and the run-resume state only move once the
        // verdict actually reached the driver.
        if let Err(e) = self.enqueue(ClientCommand::Verdict { req_id, decision }) {
            self.system_line(Self::enqueue_failure_line("permission", e));
            return;
        }
        self.prompt = None;
        // Resume the run without resetting its clock: end_waiting flips
        // Waiting → Running preserving the original started_at.
        self.run_state.end_waiting();
        self.refresh_fold_active();
        // Clear stale thinking state before post-resume streaming begins. The
        // run carried its progress through the pause, so these resets clear
        // real leftover values rather than touching defaults.
        if let Some(p) = self.run_progress_mut() {
            p.last_delta_at = None;
            p.live_block = LiveBlock::None;
            p.thinking_started_at = None;
        }
    }

    /// Resolve the startup trust verdict. Rejection also exits the local TUI,
    /// but only after the verdict was actually queued.
    pub fn resolve_trust(&mut self, accept: bool) {
        let req_id = match self.prompt.as_ref() {
            Some(PendingPrompt::Trust { req_id, .. }) => *req_id,
            _ => return,
        };
        if let Err(e) = self.enqueue(ClientCommand::TrustVerdict { req_id, accept }) {
            // The host still waits for this verdict: the request id lives on
            // inside the prompt, so nothing to restore on a failed send.
            self.system_line(Self::enqueue_failure_line("trust", e));
            return;
        }
        self.prompt = None;
        if !accept {
            self.quit = true;
        }
    }

    /// Present a wire permission request and retain its request identifier.
    /// Question tools use the interactive question card; others use the
    /// generic approval card.
    fn raise_agent_approval(&mut self, ask: ApprovalRequest, req_id: RequestId) {
        let call_id = ask.call_id.clone();
        let tool = ask.tool_name.clone();
        // Pause the spinner while the run waits on the human verdict.
        // begin_waiting preserves the run's identity and start time so the
        // verdict can resume without resetting the clock.
        self.run_state.begin_waiting();
        self.refresh_fold_active();
        self.prompt = Some(PendingPrompt::permission(req_id, vec![ask.clone()]));
        if tool == "AskUserQuestion"
            && let Some(aq) = AskQuestion::parse(&call_id, &ask.input)
        {
            if let Some(p) = self.prompt.as_mut() {
                p.set_question(Some(aq));
            }
            return;
        }
        // Malformed questions fall back to the generic card. Safety requests
        // hide persistent approval because consent cannot override them.
        let args = ask.input.to_string();
        let mut selected = self.initial_cursor(&tool);
        let (reason, source, containment_note) = if tool == ENTITLEMENT_TOOL {
            // The entitlement ask carries no gate reason — the deny-log
            // scan is why the card is up.
            ("denied during the last command".to_string(), None, None)
        } else {
            match ask.reason {
                Some(r) => (r.detail.clone(), Some(r.source), r.containment_note.clone()),
                None => ("agent wants to run this tool".to_string(), None, None),
            }
        };
        let two_option =
            tool == ENTITLEMENT_TOOL || matches!(source, Some(AskSource::SystemSafety));
        // Two-option cards cannot retain a hidden persistent choice.
        if two_option && selected == 2 {
            selected = 0;
        }
        if let Some(p) = self.prompt.as_mut() {
            p.set_approval(Some(Approval {
                tool,
                args,
                reason,
                source,
                delegation: ask.delegation,
                containment_note,
                selected,
                call_id,
                options: Vec::new(),
            }));
        }
    }

    /// Select the initial approval choice. A remembered verdict wins;
    /// automatic mode and the default both select one-time approval.
    fn initial_cursor(&self, tool: &str) -> usize {
        if let Some(kind) = self.sticky_choices.get(tool) {
            return Approval::index_for_kind(*kind);
        }
        if matches!(self.mode_cache, Some(PermissionMode::Auto)) {
            return 0;
        }
        0
    }

    /// Whether a reverse request still awaits its verdict. Idle client
    /// requests pause so they cannot compete for response frames.
    pub fn reverse_request_in_flight(&self) -> bool {
        self.prompt.as_ref().is_some_and(|p| p.is_permission())
    }

    /// Enqueue the status query, then drain startup messages by type until the
    /// trust gate resolves. Ready arrives before any trust prompt, so the drain
    /// keeps reading past it: the loop dispatches each polled message by type
    /// and stops once the trust card is raised or the status reply proves the
    /// workspace passed the gate.
    pub fn startup_handshake(&mut self, timeout: Duration) {
        if let Some(session) = self.session.as_ref() {
            match session.next_request_id() {
                Ok(req_id) => {
                    if session
                        .enqueue(ClientCommand::StatusQuery { req_id })
                        .is_err()
                    {
                        // Closed: the ConnectionLost event announces the loss.
                    }
                }
                Err(_) => self.note_request_id_exhausted(),
            }
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(message) = self
                .session
                .as_mut()
                .and_then(|session| session.poll_startup(remaining))
            else {
                break;
            };
            self.handle_agent_message(message);
            if self.pending_trust().is_some() || self.status_cache.is_some() {
                break;
            }
        }
    }

    /// Apply all available agent messages and return whether state changed.
    /// Consecutive frames rebuild the transcript as one batch;
    /// other messages first flush preceding frames to preserve order.
    pub fn poll_agent(&mut self) -> bool {
        let mut applied = false;
        let mut batch: Vec<TranscriptFrame> = Vec::new();
        let mut remaining = MAX_AGENT_MESSAGES_PER_POLL;
        let mut lookahead_used = false;
        while remaining > 0 {
            remaining -= 1;
            // Poll one owned message off the session so the session borrow
            // ends before the mutable dispatch below. The batch cap returns
            // control to terminal input even when producers remain saturated.
            let outcome = self.session.as_mut().map(|s| s.poll());
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
            // the projection writes beside it. Grant one small lookahead so
            // the pair folds in one rebuild instead of splitting across two.
            if remaining == 0 && !lookahead_used && batch.last().is_some_and(is_user_frame) {
                lookahead_used = true;
                remaining = DELIVERY_MARK_LOOKAHEAD;
            }
        }
        self.apply_frames(batch);
        // Resolve a bounded number of resume rows per poll.
        if self.resume_picker.open
            && let Some(catalog) = self.session_catalog.as_ref()
        {
            self.resume_picker.resolve_rows(catalog.as_ref(), 3);
        }
        // Refresh status while idle. Active runs and reverse requests retain
        // exclusive ownership of response frames.
        const STATUS_POLL_INTERVAL_SECS: u64 = 1;
        if !self.agent_busy()
            && !self.reverse_request_in_flight()
            && self
                .last_status_poll
                .map(|t| t.elapsed().as_secs() >= STATUS_POLL_INTERVAL_SECS)
                .unwrap_or(true)
        {
            self.last_status_poll = Some(Instant::now());
            if let Some(s) = self.session.as_ref() {
                match s.next_request_id() {
                    Ok(req_id) => {
                        if s.enqueue(ClientCommand::StatusQuery { req_id }).is_ok() {
                            // Seed the mode cache once so the status pill renders at
                            // startup. A refused enqueue stays quiet: the
                            // ConnectionLost event announces the loss.
                            if self.mode_cache.is_none() {
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
    /// own published total, not the active surface's count: the frames and
    /// transcript lengths are the parent's, and a child view on screen would
    /// otherwise mix parent state with a child-derived top.
    fn debug_render_done<F: AsRef<TranscriptFrame>>(&self, frames: &[F]) {
        if std::env::var("HICODER_DEBUG_RENDER").is_err() {
            return;
        }
        let total = self.transcript_scroll.total.get();
        tracing::warn!(
            "[done] frames={} transcript={} cap={} total={} follow={} top={} approval={} busy={}",
            frames.len(),
            self.transcript.len(),
            self.transcript_scroll.cap.get(),
            total,
            self.transcript_scroll.is_following_tail(),
            self.transcript_scroll.top_offset(total),
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
            session_id: self.session_id.clone(),
        }) {
            self.system_line(Self::enqueue_failure_line("run", e));
            return;
        }
        self.run_state.begin_cancel();
    }

    /// Recall the queued item at the cursor position into the input box.
    /// Sends QueueRemove for a live server copy. Leaves other items queued.
    pub fn recall_queued_at_cursor(&mut self) {
        let idx = self
            .queue_view
            .cursor
            .min(self.pending.len().saturating_sub(1));
        if idx >= self.pending.len() {
            return;
        }
        let Some(item) = self.remove_pending_at(idx) else {
            return;
        };
        self.queue_view
            .clamp(self.pending.len().max(self.queue_view.cursor));
        self.merge_recalled_text(item.display().to_string());
    }

    /// Delete the queued item at the cursor position without recalling it.
    /// Sends QueueRemove for a live server copy.
    pub fn delete_queued_at_cursor(&mut self) {
        let idx = self
            .queue_view
            .cursor
            .min(self.pending.len().saturating_sub(1));
        if idx >= self.pending.len() {
            return;
        }
        if self.remove_pending_at(idx).is_none() {
            return;
        }
        self.queue_view.clamp(self.pending.len());
    }

    /// Recall queued messages into the input box before the current draft.
    /// Commands remain queued. A message leaves the queue only together with
    /// its server mirror: a refused mirror removal keeps the entry queued so
    /// the input cannot duplicate on the next drain.
    pub fn pop_queued_to_input(&mut self) {
        if self
            .pending
            .iter()
            .all(|it| matches!(it, PendingItem::Command(_)))
        {
            return;
        }
        let mut keep: Vec<PendingItem> = Vec::new();
        let mut recalled: Vec<String> = Vec::new();
        let mut refused = false;
        for it in std::mem::take(&mut self.pending) {
            match it {
                PendingItem::Message(input) => {
                    let session_id = self.session_id.clone();
                    if self
                        .enqueue(ClientCommand::QueueRemove {
                            session_id,
                            id: input.id,
                        })
                        .is_ok()
                    {
                        recalled.push(input.text);
                    } else {
                        refused = true;
                        keep.push(PendingItem::Message(input));
                    }
                }
                PendingItem::ParkedMessage(input) => recalled.push(input.text),
                other => keep.push(other),
            }
        }
        self.pending = keep;
        if refused {
            self.system_line("queue: connection lost — some entries stayed queued");
        }
        if recalled.is_empty() {
            return;
        }
        // Explicit recall supersedes automatic interrupted-turn restoration.
        self.last_run_input = None;
        self.merge_recalled_text(recalled.join("\n"));
    }

    /// Insert recalled text before the current draft. The cursor remains at
    /// the draft boundary so editing can continue without losing input.
    pub(crate) fn merge_recalled_text(&mut self, text: String) {
        let draft = self.input.value().to_string();
        if draft.is_empty() {
            self.input.set(text);
        } else {
            let merged = format!("{text}\n{draft}");
            let draft_start = text.len() + 1; // past the text + newline
            self.input.set(merged);
            self.input.move_to(draft_start);
        }
    }

    /// Remove the latest submitted turn from the frame log and rebuild the
    /// transcript. Used when interruption arrives before substantive output so
    /// the user can edit and resend the restored input. The rows the frontend
    /// raised during that turn go with it: they describe work now discarded.
    pub fn rewind_to_last_user_input(&mut self) {
        let Some(start) = self.transcript.frames().iter().rposition(|sf| {
            matches!(
                sf.as_ref(),
                TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_))
            )
        }) else {
            return;
        };
        self.transcript.with_frames_mut(|log| log.truncate(start));
        self.todos.set_replaying_history(true);
        self.rebuild_transcript();
    }
}

/// Clear the previous run's interruption markers: the Interrupted notice and
/// the restore line that can precede it. Each marker names itself in the log,
/// so a turn with real output clears its lone notice while a turn that gave
/// the submission back clears both — matched by identity, not by position or
/// by the words the row happens to render.
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
fn should_preserve_interrupted_turn<F: AsRef<TranscriptFrame>>(frames: &[F]) -> bool {
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

#[path = "agent_dispatch.rs"]
mod agent_dispatch;

#[cfg(test)]
#[path = "run_control_tests.rs"]
pub(crate) mod run_control_tests;

#[cfg(test)]
#[path = "send_failure_tests.rs"]
mod send_failure_tests;
