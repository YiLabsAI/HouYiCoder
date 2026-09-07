//! Coordinates user turns between application state and the protocol client.
//! Commands travel through the client driver; returned frames form the durable
//! transcript, while streaming events update the live presentation.

use std::time::Instant;

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::extension::ENTITLEMENT_TOOL;
use houyicoder_protocol::frontend::permission::{AskSource, PermissionMode};
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalRequest, ContentBlock};
use houyicoder_protocol::frontend::session_update::SessionUpdate;

use crate::pending_queue::PendingItem;
use crate::records::{Approval, AskQuestion, TranscriptLine};

const MAX_PROJECT_FRAMES: usize = 500;
const PREPEND_BATCH: usize = 100;
use crate::state::App;
use crate::state::enums::LiveBlock;
use crate::transcript::{TranscriptFrame, chunk_text};

#[path = "run_control/projection.rs"]
mod projection;

pub use crate::agent_message::{AgentMessage, ClientCommand};

impl App {
    /// Mint a fresh request id for a wire request. Delegates to the session;
    /// None when no backend is wired (stub path).
    pub fn mint_request_id(&self) -> Option<RequestId> {
        self.session.as_ref().map(|s| s.mint_request_id())
    }

    /// Ship a command to the driver over the session's command channel.
    /// No-op when no backend is wired.
    pub fn send_cmd(&self, cmd: ClientCommand) {
        if let Some(s) = &self.session {
            s.send(cmd);
        }
    }

    /// Start a user turn, steer input to the viewed child, or queue it while
    /// another turn is active. New turns render an immediate user echo before
    /// the durable transcript arrives.
    pub fn spawn_run(&mut self, input: String) {
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
            } else {
                if let Some(view) = self.teammate_view.as_mut() {
                    view.transcript.push(TranscriptLine::User(input.clone()));
                    view.pending_echo = Some(input.clone());
                    self.transcript_scroll.follow_tail = true;
                }
                // Invalidate cached rows after the optimistic echo.
                self.bump_transcript_version();
                self.send_cmd(ClientCommand::InjectToChild {
                    child_sid,
                    text: input,
                });
            }
            return;
        }
        // Active turns park new input locally. Promotion keeps at most one
        // server-side copy while preserving queue order.
        if self.agent_busy {
            self.pending.push(PendingItem::ParkedMessage(input.clone()));
            self.promote_next_pending();
            return;
        }
        let Some(req_id) = self.session.as_ref().map(|s| s.mint_request_id()) else {
            return;
        };
        // Only errors matching this request terminate the active run.
        self.active_run_req_id.set(Some(req_id));
        // Preserve the submitted input in case interruption restores the turn.
        self.last_run_input = Some(input.clone());
        self.push_transcript_line(TranscriptLine::User(input.clone()));
        self.agent_busy = true;
        self.run_started = Some(Instant::now());
        self.last_delta_at = None;
        self.displayed_tokens.set(0);
        self.thinking_started_at = None;
        self.live_block = LiveBlock::None;
        let session_id = self.session_id.clone();
        let content = vec![ContentBlock::Text { text: input }];
        let disabled_skills = self.skill_disabled.clone();
        self.send_cmd(ClientCommand::SendMessage {
            req_id,
            session_id,
            content,
            disabled_skills,
        });
    }

    /// Consume the pending queue head in first-in, first-out order. Clean
    /// completion may start the next turn; other outcomes leave input parked.
    /// At most one parked message is promoted to the server queue. Returns
    /// false when no item is available.
    pub fn drain_pending_head(&mut self) -> bool {
        let Some(item) = self.pending.first().cloned() else {
            return false;
        };
        self.pending.remove(0);
        match item {
            PendingItem::Command(text) => self.run_slash_text(&text),
            PendingItem::Message(head) => {
                // Remove the stale server copy before starting a fresh run.
                let session_id = self.session_id.clone();
                self.send_cmd(ClientCommand::QueueRemove {
                    session_id,
                    text: head.clone(),
                });
                self.spawn_run(head);
                self.promote_next_pending();
                true
            }
            PendingItem::ParkedMessage(text) => {
                self.spawn_run(text);
                self.promote_next_pending();
                true
            }
        }
    }

    /// Resolve the active approval, clear its interface state, and send the
    /// verdict as the matching reverse response. No-op when no request awaits
    /// a decision.
    pub fn resolve_current_approval(&mut self, decision: ApprovalDecision) {
        let Some(req_id) = self.pending_permission_req_id.take() else {
            return;
        };
        self.pending_approvals.clear();
        self.approval = None;
        self.ask_question = None;
        // Keep the resumed run busy without resetting its displayed tokens.
        self.agent_busy = true;
        self.run_started = Some(Instant::now());
        self.last_delta_at = None;
        // Clear stale thinking state before post-resume streaming begins.
        self.live_block = LiveBlock::None;
        self.thinking_started_at = None;
        self.send_cmd(ClientCommand::Verdict { req_id, decision });
    }

    /// Resolve the startup workspace-trust request. Acceptance continues the
    /// session and persists the trusted path; rejection ends the session.
    pub fn resolve_trust(&mut self, accept: bool) {
        let Some(req_id) = self.pending_trust_req_id.take() else {
            return;
        };
        self.pending_trust = None;
        self.send_cmd(ClientCommand::TrustVerdict { req_id, accept });
    }

    /// Present a wire permission request and retain its request identifier.
    /// Question tools use the interactive question card; others use the
    /// generic approval card.
    fn raise_agent_approval(&mut self, ask: ApprovalRequest) {
        let call_id = ask.call_id.clone();
        let tool = ask.tool_name.clone();
        // Pause the spinner while the run waits on the human verdict.
        self.agent_busy = false;
        self.run_started = None;
        self.pending_approvals = vec![ask.clone()];
        if tool == "AskUserQuestion"
            && let Some(aq) = AskQuestion::parse(&call_id, &ask.input)
        {
            self.ask_question = Some(aq);
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
        self.approval = Some(Approval {
            tool,
            args,
            reason,
            source,
            delegation: ask.delegation,
            containment_note,
            selected,
            call_id,
            options: Vec::new(),
        });
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
        self.pending_permission_req_id.get().is_some()
    }

    /// Apply all available agent messages and return whether state changed.
    /// Consecutive frames are projected as one batch to keep replay linear;
    /// other messages first flush preceding frames to preserve order.
    pub fn poll_agent(&mut self) -> bool {
        let mut applied = false;
        let mut batch: Vec<TranscriptFrame> = Vec::new();
        loop {
            // Poll one owned message off the session so the session borrow
            // ends before the mutable dispatch below.
            let msg = self.session.as_mut().and_then(|s| s.poll());
            match msg {
                Some(AgentMessage::Frame(frame)) => {
                    batch.push(frame);
                    applied = true;
                }
                Some(m) => {
                    self.apply_frames(batch.drain(..));
                    self.handle_agent_message(m);
                    applied = true;
                }
                None => break,
            }
        }
        self.apply_frames(batch);
        // Resolve a bounded number of resume rows per poll.
        if self.resume_picker.open
            && let Some(lister) = self.session_lister.as_ref()
        {
            let mut resolved_count = 0;
            for i in 0..self.resume_picker.rows.len() {
                if resolved_count >= 3 {
                    break;
                }
                if !self.resume_picker.resolved.contains(&i) {
                    lister.resolve_detail(&mut self.resume_picker.rows[i]);
                    self.resume_picker.resolved.insert(i);
                    resolved_count += 1;
                    // Newest rows resolve first, so duplicate titles hide older rows.
                    let title = self.resume_picker.rows[i].title.clone();
                    if !self.resume_picker.seen_titles.insert(title) {
                        self.resume_picker.rows[i].hidden = true;
                    }
                }
            }
        }
        // Refresh status while idle. Active runs and reverse requests retain
        // exclusive ownership of response frames.
        const STATUS_POLL_INTERVAL_SECS: u64 = 1;
        if !self.agent_busy
            && !self.reverse_request_in_flight()
            && self
                .last_status_poll
                .map(|t| t.elapsed().as_secs() >= STATUS_POLL_INTERVAL_SECS)
                .unwrap_or(true)
        {
            self.last_status_poll = Some(Instant::now());
            if let Some(s) = self.session.as_ref() {
                s.request_status();
                // Seed the mode cache once so the status pill renders at startup.
                if self.mode_cache.is_none() {
                    s.request_permission_mode();
                }
            }
        }
        applied
    }

    /// Env-gated render diagnostic at each turn boundary.
    fn debug_render_done(&self, frames: &[TranscriptFrame]) {
        if std::env::var("HICODER_DEBUG_RENDER").is_err() {
            return;
        }
        tracing::warn!(
            "[done] frames={} transcript={} cap={} total={} follow={} top={} approval={} busy={}",
            frames.len(),
            self.transcript.len(),
            self.transcript_scroll.cap.get(),
            self.transcript_scroll.total.get(),
            self.transcript_scroll.follow_tail,
            self.transcript_scroll
                .top_offset(self.transcript_display_rows()),
            self.approval.is_some(),
            self.agent_busy,
        );
    }

    /// Abort the active run. The driver propagates cancellation through the
    /// execution pipeline and records the request for auditing.
    pub fn abort_run(&mut self) {
        // Keep cancellation visible until run completion clears the state.
        self.cancelling = true;
        self.send_cmd(ClientCommand::AbortRun {
            session_id: self.session_id.clone(),
        });
    }

    /// Recall queued messages into the input box before the current draft.
    /// Commands remain queued, and server-side message copies are removed.
    pub fn pop_queued_to_input(&mut self) {
        let messages: Vec<String> = self
            .pending
            .iter()
            .filter_map(|it| match it {
                PendingItem::Message(t) | PendingItem::ParkedMessage(t) => Some(t.clone()),
                PendingItem::Command(_) => None,
            })
            .collect();
        if messages.is_empty() {
            return;
        }
        // Drain only message items; keep commands in place so they stay queued.
        let mut keep: Vec<PendingItem> = Vec::new();
        for it in std::mem::take(&mut self.pending) {
            match &it {
                PendingItem::Message(text) => {
                    self.send_cmd(ClientCommand::QueueRemove {
                        session_id: self.session_id.clone(),
                        text: text.clone(),
                    });
                }
                PendingItem::ParkedMessage(_) => {}
                PendingItem::Command(_) => {
                    keep.push(it);
                }
            }
        }
        self.pending = keep;
        // Explicit recall supersedes automatic interrupted-turn restoration.
        self.last_run_input = None;
        let text = messages.join("\n");
        self.merge_recalled_text(text);
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
    /// the user can edit and resend the restored input.
    pub fn rewind_to_last_user_input(&mut self) {
        let Some(start) = self.frames.iter().rposition(|f| {
            matches!(
                f,
                TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_))
            )
        }) else {
            return;
        };
        self.frames.truncate(start);
        self.rebuild_transcript();
    }
}

/// Whether an interrupted turn must remain submitted. Assistant output, tool
/// calls, and reasoning preserve the turn; a user-only turn may be restored.
/// Missing user context preserves the turn conservatively.
fn should_preserve_interrupted_turn(frames: &[TranscriptFrame]) -> bool {
    let Some(start) = frames.iter().rposition(|f| {
        matches!(
            f,
            TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_))
        )
    }) else {
        return true;
    };
    frames[start + 1..].iter().any(|f| match f {
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
mod run_control_tests;

#[cfg(test)]
#[path = "spawn_run_queue_tests.rs"]
mod spawn_run_queue_tests;
