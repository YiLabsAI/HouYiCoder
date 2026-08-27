//! Real agent-loop wiring for the TUI. The TUI holds a protocol Client (L4)
//! and drives the agent turn over the wire: spawn_run ships a MessageSend
//! request to a long-lived client-driver task; the driver drains server
//! frames, routes acpx/llm/* token deltas to AgentMessage::Delta, raises
//! mid-turn permission asks as AgentMessage::PermissionAsk, and ships the
//! final outcome as AgentMessage::Done. The transcript is rebuilt from the
//! ordered wire frame stream the driver accumulates (session/update chunks
//! and acpx/context/* audit). The wire frames are the source of truth for
//! the projection, so the TUI never imports engine event types.
//!
//! Streaming rides the wire: the composition root installs a live delta
//! sink on the shared runner that fires acpx/llm/* notifications during the
//! server's run; the driver routes them to AgentMessage::Delta. Run,
//! permission, and streaming all cross the wire; the transcript rides the
//! accumulated wire frames. The TUI holds no engine handle and imports no
//! ports live types.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::run::{ApprovalDecision, ApprovalRequest, ContentBlock};

use crate::pending_queue::PendingItem;
use crate::records::{Approval, TranscriptLine};

const MAX_PROJECT_FRAMES: usize = 500;
const PREPEND_BATCH: usize = 100;
use crate::state::App;
use crate::transcript::TranscriptFrame;

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

    /// Start a new user turn by shipping a MessageSend to the driver task. The
    /// driver forwards it over the wire; the server drives runner.run (firing
    /// the shared live sink so streamed deltas arrive here as Delta); the final
    /// outcome arrives as Done. Sets agent_busy so a second Enter queues. The
    /// user echo lands immediately and is replaced consistently when the
    /// durable transcript is rebuilt on Done.
    pub fn spawn_run(&mut self, input: String) {
        // Steering: when the user is viewing a child (teammate view), the
        // typed input routes to that child's inbox, not the parent. Works
        // mid-parent-run (the child is the running task); the child drains
        // it at its next turn boundary. No parent echo — the text is a
        // steering message, not a parent turn; the viewed child transcript
        // shows it when drained.
        if let Some(view) = self.teammate_view.as_ref().filter(|_| !input.is_empty()) {
            self.send_cmd(ClientCommand::InjectToChild {
                child_sid: view.child_sid.clone(),
                text: input,
            });
            return;
        }
        // Queue path: when a run is in flight, the new input is mirrored to
        // pending (the queue view) + shipped as a session/inject
        // notification so the server enqueues it for mid-turn injection at
        // the next turn boundary. The drive loop drains it + the model sees
        // it on its next call (Path A); if the run ends first, the run-end
        // drain spawns it as a follow-up run (Path B). The active_run_req_id
        // stays set so a wire Error for IT still routes as a run failure.
        if self.agent_busy {
            // Barrier: a pending Command or ParkedMessage ahead will swap/reset
            // the session or has no server copy. A message enqueued after it
            // stays host-side only (no InjectUser) so it neither outlives a
            // swap/reset nor leapfrogs a parked message mid-turn. Lifts when
            // the blocking item drains.
            let barrier = self.barrier_active();
            if barrier {
                self.pending.push(PendingItem::ParkedMessage(input.clone()));
            } else {
                self.pending.push(PendingItem::Message(input.clone()));
                let session_id = self.session_id.clone();
                self.send_cmd(ClientCommand::InjectUser {
                    session_id,
                    text: input,
                });
            }
            return;
        }
        // Mint the request id only on the real-spawn path (the queue path
        // above ships nothing). Borrow ends at the Option<RequestId>, so the
        // mutable self access below is clean.
        let Some(req_id) = self.session.as_ref().map(|s| s.mint_request_id()) else {
            return;
        };
        // Track THIS run's req_id so a wire Error for it routes as a run
        // failure (Done{Err}); a non-matching Error (a permission verb)
        // routes as a per-request system line instead.
        self.active_run_req_id.set(Some(req_id));
        // Stash the input so an abort-with-no-real-content can restore it to
        // the input box. Not set on the queue path: a queued input is not the
        // in-flight run's origin.
        self.last_run_input = Some(input.clone());
        self.push_transcript_line(TranscriptLine::User(input.clone()));
        self.agent_busy = true;
        self.run_started = Some(std::time::Instant::now());
        self.last_delta_at = None;
        self.displayed_tokens.set(0);
        self.thinking_started_at = None;
        self.live_block = crate::state::enums::LiveBlock::None;
        let session_id = self.session_id.clone();
        let content = vec![ContentBlock::Text { text: input }];
        self.send_cmd(ClientCommand::SendMessage {
            req_id,
            session_id,
            content,
        });
    }

    /// Drain the head of the pending queue. Called from the event loop's idle
    /// guard (idle_drain), which gates on a clean run end (FinalOutput); bound
    /// to the consume action (not the idle condition) so the per-frame poll
    /// stays silent. Returns false when the queue is empty.
    ///
    /// A queued message auto-sends as the next turn after a clean run end (the
    /// user got their answer, drain FIFO). An interrupt/error parks the queue
    /// (idle_drain's gate holds) for the user to pop via Esc + edit. A
    /// permission pause is not idle (reverse_request_in_flight holds), so the
    /// drain does not fire there. Strict FIFO: the head always goes first, so
    /// a Command behind a parked message waits its turn (no starvation, but
    /// also no head-of-line skip).
    ///
    /// Batch: when the head is a Message, consecutive Messages behind it are
    /// InjectUser'd into the new run's input_queue (not spawned as separate
    /// runs). The drive_loop drains them at the next turn boundary (appends as
    /// user messages -- model sees them on call 2+), so N queued messages send
    /// as ONE run, not N runs. The rest stay in pending (Message); QueueConsumed
    /// removes them when the drive_loop drains them. A 1-turn run (no turn
    /// boundary) leaves them un-consumed -> idle_drain drains them next
    /// (one-by-one, same as the no-batch path). Stops at the first non-Message
    /// (Command/ParkedMessage) -- those drain singly (slash needs per-command
    /// error isolation; a ParkedMessage has no server copy to InjectUser).
    pub fn drain_pending_head(&mut self) -> bool {
        let Some(item) = self.pending.first().cloned() else {
            return false;
        };
        // Remove the head only; the batch loop below leaves consecutive
        // Messages in pending (removed by QueueConsumed when consumed).
        self.pending.remove(0);
        match item {
            PendingItem::Command(text) => self.run_slash_text(&text),
            PendingItem::Message(head) => {
                // Drop the head's stale server copy (the prior run's input_queue
                // was cleared at finalize) + start a fresh run with it.
                let session_id = self.session_id.clone();
                self.send_cmd(ClientCommand::QueueRemove {
                    session_id: session_id.clone(),
                    text: head.clone(),
                });
                self.spawn_run(head);
                // Batch consecutive Messages behind the head into the new run.
                // Index-iterate (do not remove) so QueueConsumed can drop them
                // when the drive_loop drains; a 1-turn run leaves them for the
                // next idle_drain (one-by-one).
                let mut i = 0;
                while i < self.pending.len() {
                    if let PendingItem::Message(t) = &self.pending[i] {
                        self.send_cmd(ClientCommand::InjectUser {
                            session_id: session_id.clone(),
                            text: t.clone(),
                        });
                        i += 1;
                    } else {
                        break;
                    }
                }
                true
            }
            PendingItem::ParkedMessage(text) => {
                // No server copy (barrier'd or orphaned): spawn a fresh
                // run directly. No QueueRemove -- there is no server copy.
                self.spawn_run(text);
                true
            }
        }
    }

    /// Resolve the currently-shown approval with the user's verdict and ship
    /// it to the driver task, which forwards it as the matching reverse
    /// response so the server resumes the turn. The caller (keys.rs
    /// handle_approval or ask_question_keys) builds the full wire decision
    /// (call_id, approved, optional edited input, scope); this method pairs it
    /// with the pending reverse-request req_id, clears the card, and ships
    /// it. Over the wire the server (not the TUI) drives runner.resume and
    /// records the PermissionDecision audit event.
    pub fn resolve_current_approval(&mut self, decision: ApprovalDecision) {
        let Some(req_id) = self.pending_permission_req_id.take() else {
            return;
        };
        self.pending_approvals.clear();
        self.approval = None;
        self.ask_question = None;
        // The resume is now in flight on the server; keep the run marked busy so
        // the spinner animates until the final Done lands. displayed_tokens is
        // NOT reset here: live text persists across the approval, so zeroing
        // it would replay the count-up animation mid-turn.
        self.agent_busy = true;
        self.run_started = Some(std::time::Instant::now());
        self.last_delta_at = None;
        // Clear the live block AND the 2s thinking-min-display window so a
        // stale Thinking from before the approval does not linger during the
        // resume gap; the first post-resume delta sets both fresh. Without
        // clearing thinking_started_at the spinner verb stays "Thinking" for
        // up to 2s after a fast approval even though no reasoning is flowing.
        self.live_block = crate::state::enums::LiveBlock::None;
        self.thinking_started_at = None;
        self.send_cmd(ClientCommand::Verdict { req_id, decision });
    }

    /// Populate the approval card from a wire permission ask and stash the
    /// reverse-request req_id so resolve_current_approval can pair the verdict.
    /// When the tool is AskUserQuestion, the input is parsed into an
    /// interactive question card instead of the generic approval popup.
    fn raise_agent_approval(&mut self, ask: ApprovalRequest) {
        let call_id = ask.call_id.clone();
        let tool = ask.tool_name.clone();
        // Pause the spinner while the run waits on the human verdict.
        self.agent_busy = false;
        self.run_started = None;
        self.pending_approvals = vec![ask.clone()];
        if tool == "AskUserQuestion"
            && let Some(aq) = crate::records::AskQuestion::parse(&call_id, &ask.input)
        {
            self.ask_question = Some(aq);
            return;
        }
        // Malformed input for AskUserQuestion, or a different tool: use the
        // generic approval card so the user can still approve or reject. The
        // reason the gate produced travels the wire AskReason; surface its
        // detail + source so the card reads "why am I being asked" and hides
        // the remember option when the source is a protected-path check
        // (consent cannot override it).
        let args = ask.input.to_string();
        let mut selected = self.initial_cursor(&tool);
        let (reason, source, containment_note) = match ask.reason {
            Some(r) => (r.detail.clone(), Some(r.source), r.containment_note.clone()),
            None => ("agent wants to run this tool".to_string(), None, None),
        };
        let remember_hidden = matches!(
            source,
            Some(houyicoder_protocol::frontend::permission::AskSource::SystemSafety)
        );
        // A protected-path ask hides Yes-don't-ask; clamp a sticky AllowAlways
        // preselect down to Yes so the cursor never lands on a hidden option.
        if remember_hidden && selected == 2 {
            selected = 0;
        }
        self.approval = Some(Approval {
            tool,
            args,
            reason,
            source,
            containment_note,
            selected,
            call_id,
            options: Vec::new(),
        });
    }

    /// Pick the initial cursor for a fresh approval popup. Priority: a sticky
    /// last-used verdict for this tool (matched by identity, not list
    /// position); then YOLO when the permission mode auto-approves (Auto or
    /// Bypass) focuses the quickest approve; otherwise index-0. A configured
    /// per-tool default would sit between sticky and YOLO, but no config
    /// schema exists for it yet.
    fn initial_cursor(&self, tool: &str) -> usize {
        if let Some(kind) = self.sticky_choices.get(tool) {
            return crate::records::Approval::index_for_kind(*kind);
        }
        use houyicoder_protocol::frontend::permission::PermissionMode as M;
        if matches!(self.mode_cache, Some(M::Auto)) {
            return 0;
        }
        0
    }

    /// Drain any finished agent message off the session. Returns true when at
    /// least one message was applied (so the caller knows to redraw). Drains all
    /// pending messages in one call so a burst of streamed Deltas followed by a
    /// Done is processed atomically.
    /// True when a reverse-request (a permission ask, including an
    /// AskUserQuestion) is in flight and its verdict has not been sent. The
    /// status poll and any idle client request must suppress while this is set
    /// so their frames do not compete with the run/resume loop frame reads.
    /// This is the true invariant; the prior guard approximated it with
    /// agent_busy plus approval, which AskUserQuestion satisfies neither (it
    /// does not set approval and runs with agent_busy false), so a status tick
    /// landed mid-ask and deadlocked.
    pub fn reverse_request_in_flight(&self) -> bool {
        self.pending_permission_req_id.get().is_some()
    }

    pub fn poll_agent(&mut self) -> bool {
        let mut applied = false;
        // Consecutive frames accumulate and land as one batch: a resume
        // replays the whole session history through this drain, and projecting
        // per frame is quadratic in the history size. Any other message flushes
        // the batch first so it observes the frames that preceded it.
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
        // Progressive resume resolution: resolve a few unresolved rows per
        // frame so the picker fills in titles + last-active times top-to-
        // bottom. Each resolve is one log-head read; batching 3/frame keeps
        // the poll cheap while making the list feel instant.
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
                    // Lazy dedup: rows are sorted newest-first + resolved
                    // top-to-bottom, so the first occurrence of each title
                    // is the newest. If this row's just-filled real title
                    // duplicates one already seen, hide this older row.
                    let title = self.resume_picker.rows[i].title.clone();
                    if !self.resume_picker.seen_titles.insert(title) {
                        self.resume_picker.rows[i].hidden = true;
                    }
                }
            }
        }
        // Periodic status refresh: poll the server every second while idle (no
        // run or approval in flight) so the per-frame status bar + the /sandbox
        // + /compact read a recent snapshot without an engine call. Suppressed
        // during a run or a pending approval so the status query frame never
        // competes with the run/resume loop's frame reads — the snapshot does
        // not change mid-run anyway (usage lands on Done). Cheap on the
        // in-memory carrier; the result updates status_cache silently unless a
        // /status command is pending.
        const STATUS_POLL_INTERVAL_SECS: u64 = 1;
        if !self.agent_busy
            && !self.reverse_request_in_flight()
            && self
                .last_status_poll
                .map(|t| t.elapsed().as_secs() >= STATUS_POLL_INTERVAL_SECS)
                .unwrap_or(true)
        {
            self.last_status_poll = Some(std::time::Instant::now());
            if let Some(s) = self.session.as_ref() {
                s.request_status();
                // The mode pill reads mode_cache, which stays None until the
                // first explicit /mode or /model query. Seed it once on the
                // idle poll so the pill renders from session start; later /mode
                // cycles already update the cache via PermissionModeResult, so
                // this only fills the initial gap.
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

    /// Abort the in-flight run, if any. When a client is wired, also sends a
    /// RunCancel wire request for the server's audit trail. The actual token
    /// fire is always direct (the TUI shares the Arc<Runner>) so the run
    /// resolves promptly: the token propagates through resolve_turn ->
    /// execute_partitioned into ToolCtx, and a tool that honors ctx.cancel
    /// (Grep/Glob) short-circuits its spawn_blocking walk; a tool that does
    /// not is still cut off because the partition select! races the batch
    /// against cancellation. The wire request is for bookkeeping, not the
    /// abort itself.
    pub fn abort_run(&mut self) {
        // Mark the cancel in flight so the UI can show a cancelling state
        // until the run resolves Interrupted on Done. Cleared in the Done
        // handler. Idempotent: re-firing before the run settles is a no-op
        // (the server aborts once; extra notifications are harmless).
        self.cancelling = true;
        self.send_cmd(ClientCommand::AbortRun {
            session_id: self.session_id.clone(),
        });
    }

    /// Pop the queue head back to the input box for editing. When no run is
    /// in flight and the queue holds pending inputs (parked from a prior
    /// interrupt/error), Esc pops the head into the input box instead of
    /// doing nothing (no running task to abort). Also pops it from the host
    /// queue (wire QueueRemove for a Message with a live server copy) so a
    /// follow-up run does
    /// not re-inject it. No-op when the queue is empty.
    pub fn pop_queued_to_input(&mut self) {
        let Some(item) = self.pending.first().cloned() else {
            return;
        };
        self.pending.remove(0);
        // The pop is the user's explicit recall: it supersedes the aborted
        // run's origin stash, so the Done(Interrupted) no-content restore must
        // not re-fill the input box with the old origin and lose the popped
        // text (which is already removed from the queue).
        self.last_run_input = None;
        match &item {
            // A message with a live server copy (InjectUser'd): recall drops
            // it from the wire queue too so a follow-up run does not re-inject.
            PendingItem::Message(text) => {
                self.send_cmd(ClientCommand::QueueRemove {
                    session_id: self.session_id.clone(),
                    text: text.clone(),
                });
                self.input.set(text.clone());
            }
            // A parked message has no server copy: recall just re-fills the
            // input box (no QueueRemove -- nothing to drop on the wire).
            PendingItem::ParkedMessage(text) => self.input.set(text.clone()),
            // A command is local-only (never InjectUser'd), so recall just
            // re-fills the input box with the raw text (incl. the slash).
            PendingItem::Command(text) => self.input.set(text.clone()),
        }
    }

    /// Rewind the frame log to just before the last user message — drop the
    /// user echo and everything after it (agent chunks, tool calls, thoughts
    /// from this turn) — then rebuild the transcript so the user echo and any
    /// partial turn content disappear. The rewind-on-
    /// cancel behavior: when the user interrupted before any real content, undo the
    /// submit so they can edit and resend. No-op when no user echo is in the
    /// frames (the caller already checked run_produced_real_content is false).
    pub fn rewind_to_last_user_input(&mut self) {
        use houyicoder_protocol::frontend::session_update::SessionUpdate;
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

/// Whether the run that just finished produced any real assistant-originated
/// content after the last user message. Used to decide auto-restore on
/// interrupt: a turn that streamed nothing back gives the input back so the
/// user can edit and resend. Streaming deltas are not on the wire (they ride
/// the shared live sink), so the wire stream carries only the authoritative
/// agent message chunk + tool calls + thought chunks.
fn run_produced_real_content(frames: &[TranscriptFrame]) -> bool {
    let Some(start) = frames.iter().rposition(|f| {
        matches!(
            f,
            TranscriptFrame::Session(SessionUpdate::UserMessageChunk(_))
        )
    }) else {
        return true;
    };
    use houyicoder_protocol::frontend::session_update::SessionUpdate;
    frames[start + 1..].iter().any(|f| match f {
        TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(chunk)) => {
            !crate::transcript::chunk_text(chunk).is_empty()
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
