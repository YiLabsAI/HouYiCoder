//! Maps inbound agent messages to application state transitions.
//! Run completion is delegated so this module remains focused on dispatch.

#[path = "agent_dispatch/run_completion.rs"]
mod run_completion;

#[path = "agent_dispatch/memory.rs"]
mod memory;

use std::iter;
use std::time::Instant;

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::SessionId as FrontendSessionId;
use houyicoder_protocol::frontend::compact::CompactReply;
use houyicoder_protocol::frontend::context::ContextBreakdown;
use houyicoder_protocol::frontend::session_update::{SessionUpdate, ToolCallStatus};

use crate::agent_message::{
    AgentStatusSnapshot, ClientCommand, ConnectionEvent, FleetEntry, ServerEvent, ServerRequest,
    ServerResponse, SessionMessage,
};
use crate::command::render::{render_permission_rules_wire, render_trajectory_wire};
use crate::composition::suggestions_for;
use crate::pending_prompt::PendingPrompt;
use crate::pending_queue::PendingItem;
use crate::records::{ContextDrillDown, ContextView, TranscriptLine};
use crate::state::enums::LiveBlock;
use crate::state::{App, BashProgress};
use crate::terminal_title::sync as sync_terminal_title;
use crate::transcript::frame_payload::chunk_text;
use crate::transcript::{TranscriptFrame, transcript_from_frames};

impl App {
    /// Apply an inbound agent message to application state. Dispatch follows
    /// the protocol's four directions: connection lifecycle, request
    /// responses, server events, and server-initiated requests.
    pub fn handle_agent_message(&mut self, msg: SessionMessage) {
        match msg {
            SessionMessage::Connection(event) => match event {
                ConnectionEvent::Ready => {
                    // The Hello handshake succeeded. Ready reflects the
                    // confirmed handshake, not the connection object existing,
                    // so a failed Hello leaves the status at Connecting until
                    // the loss lands. The transition is constrained: a late
                    // confirmation cannot revive a lost connection.
                    if let Some(s) = self.session.as_mut() {
                        s.mark_ready();
                    }
                }
                ConnectionEvent::Lost { cause, not_sent } => {
                    // The driver is gone: no reply can land for anything in
                    // flight. End the active run if one is live, and sweep
                    // pending pane marks instead of leaving a mark that
                    // refuses its switch forever. Server state for in-flight
                    // mutations is unknown, so no per-action failure line is
                    // written — one generic connection-loss line covers all.
                    self.apply_connection_loss(cause, not_sent);
                }
            },
            SessionMessage::Response { request, response } => {
                self.apply_response(request, response);
            }
            SessionMessage::Event(event) => self.apply_server_event(event),
            SessionMessage::Request { request, payload } => {
                self.apply_server_request(request, payload);
            }
        }
    }

    /// Settle a connection loss, returning whether this call performed the
    /// non-Lost → Lost cleanup. The first-cause-wins transition gates the
    /// work, so an announced death followed by a closed channel (or a repeat
    /// closed observation) neither overwrites the cause nor repeats the
    /// completion. A session-less App (the no-backend path) has no cause to
    /// record but still sweeps that observation.
    ///
    /// not_sent lists request ids the driver can prove it never attempted;
    /// the active run and memory actions whose ids appear in it are not sent
    /// (never left this process). Every other in-flight request is
    /// conservatively unknown — a write or flush may have delivered the frame.
    pub(crate) fn apply_connection_loss(
        &mut self,
        cause: String,
        not_sent: Vec<RequestId>,
    ) -> bool {
        let first = match self.session.as_mut() {
            Some(s) => s.mark_lost(cause.clone()),
            // No connection exists: nothing was lost twice, so run the sweep
            // for this observation. A synthetic loss on a session-less App
            // (tests, the stub path) still clears run and pane state.
            None => true,
        };
        if !first {
            return false;
        }
        // Distinguish not sent (provably never attempted) from unknown (a
        // write or flush may have partially delivered). The run's completion
        // line tells the user which: a not sent run never left the process;
        // an unknown run may have reached the server with no reply coming.
        let run_req = self.active_run_req_id();
        let run_not_sent = run_req.is_some_and(|r| not_sent.contains(&r));
        self.memory.clear_pending();
        self.skills_pane.clear_pending();
        let run_line = if run_not_sent {
            "run not sent — connection failed before the request reached the transport"
        } else {
            cause.as_str()
        };
        if run_req.is_some() {
            // Settle the active run: the loss is its terminal outcome.
            self.handle_run_completion(Err(run_line.to_string()));
        } else {
            // No run is in flight, so the loss is a notice only — it must not
            // touch run-final state or demote queued input, which a prior
            // settle owns. The line keeps the shape a settled loss would.
            self.system_line(format!("agent error: {run_line}"));
        }
        true
    }

    /// Fill the teammate view with the fetched child transcript. A pending
    /// optimistic echo (a steering message sent before the child drained
    /// it) is preserved across a refetch that lands before the durable
    /// line, so the echo does not vanish mid-turn; once the durable User
    /// line appears, the echo clears. follow_tail is left as-is so a live
    /// refetch does not yank a scrolled-up user.
    fn fill_teammate_view(&mut self, child_sid: &str, mut folded: Vec<TranscriptLine>) {
        let Some(view) = self.teammate_view.as_mut() else {
            return;
        };
        if view.child_sid != child_sid {
            return;
        }
        if let Some(echo) = view.pending_echo.clone()
            && !folded
                .iter()
                .any(|l| matches!(l, TranscriptLine::User(t) if t == &echo))
        {
            folded.push(TranscriptLine::User(echo));
        } else {
            view.pending_echo = None;
        }
        view.transcript = folded;
        // The render cache keys on transcript_version; the fill swapped the
        // child rows, so bump or the cache holds the prior child snapshot
        // (or the parent rows when this is the first fill after entering).
        self.bump_transcript_version();
    }

    /// The placeholder line shown when a child transcript fetch returns no
    /// frames. A running child with no log yet reads "starting" (the first
    /// turn has not landed); anything else reads as a real fetch failure, so
    /// the error is not hidden behind a "starting" label.
    fn empty_child_transcript_line(&self, child_sid: &str) -> TranscriptLine {
        let starting = self
            .fleet_entry(child_sid)
            .map(|e| e.completed.is_none())
            .unwrap_or(false);
        let msg = if starting {
            "child starting"
        } else {
            "child transcript unavailable"
        };
        TranscriptLine::System(msg.into())
    }

    /// Apply a server-originated event: durable frames, streaming deltas,
    /// queue and progress notifications.
    fn apply_server_event(&mut self, event: ServerEvent) {
        match event {
            ServerEvent::Frame(frame) => {
                self.apply_frames(iter::once(frame));
            }
            ServerEvent::Delta { text } => {
                // Assistant text is now the active streaming block: the spinner
                // verb must read Working, not Thinking (a sticky "reasoning
                // ever streamed" test would lock it to Thinking for the rest
                // of the turn even while text is streaming).
                if let Some(p) = self.run_progress_mut() {
                    p.live_assistant_text.push_str(&text);
                    p.live_block = LiveBlock::Responding;
                    p.live_active = true;
                    p.last_delta_at = Some(Instant::now());
                }
                // Do NOT re-pin to the tail per delta: the draw already pins
                // to the new tail when follow_tail is true, and a user who
                // scrolled up to re-read history must stay where they scrolled.
            }
            ServerEvent::ReasoningDelta { text } => {
                if let Some(p) = self.run_progress_mut() {
                    if p.live_reasoning_text.is_empty() && !text.is_empty() {
                        p.thinking_started_at = Some(Instant::now());
                    }
                    p.live_reasoning_text.push_str(&text);
                    // Reasoning is the active streaming block: the spinner verb
                    // reads Thinking while this holds (until an assistant-text
                    // Delta or a tool start flips it away).
                    p.live_block = LiveBlock::Thinking;
                    p.live_active = true;
                    p.last_delta_at = Some(Instant::now());
                }
            }
            ServerEvent::ToolProgress {
                call_id,
                elapsed_secs,
                lines,
            } => {
                // A long-running tool ticks elapsed (+ optional stdout line
                // count when the backend streams). The chip render reads
                // this map + running_tools to append (Ns) / (Ns · M lines)
                // after 2s. Only meaningful while the call is in flight;
                // finish_tool clears it when the result lands.
                if let Some(p) = self.run_progress_mut()
                    && p.running_tools.contains(&call_id)
                {
                    p.bash_progress.insert(
                        call_id,
                        BashProgress {
                            elapsed_secs,
                            lines,
                        },
                    );
                }
            }
            ServerEvent::QueuedInputCommitted { inputs } => {
                // A committed mid-turn input makes the original submission no
                // longer eligible for no-output rollback.
                if !inputs.is_empty() {
                    self.last_run_input = None;
                }
                // Remove exact committed inputs before promoting the next head.
                // Stable identity prevents a delayed event from removing newer text.
                for input in inputs {
                    if let Some(pos) = self.pending.iter().position(|it| {
                        matches!(
                            it,
                            PendingItem::Message(current) | PendingItem::ParkedMessage(current)
                                if current.id == input.id
                        )
                    }) {
                        self.pending.remove(pos);
                    }
                }
                self.promote_next_pending();
            }
            ServerEvent::MemoryChanged {
                id,
                origin,
                causality,
                changes,
            } => self.show_memory_changes(&id, origin, causality, &changes),
            ServerEvent::SystemLine { text } => {
                // A runtime notice the agent loop surfaced (e.g. an overflow
                // the catalog could not self-heal). Render verbatim as a
                // transcript system line.
                self.system_line(text);
            }
            ServerEvent::AgentStatus {
                agent_id,
                subagent_type,
                turn,
                tokens,
                tool_uses,
                last_activity,
                completed,
            } => self.apply_agent_status(AgentStatusSnapshot {
                agent_id,
                subagent_type,
                turn,
                tokens,
                tool_uses,
                last_activity,
                completed,
            }),
        }
    }

    /// Apply one child status snapshot to the fleet footer and the teammate
    /// view's live refetch.
    fn apply_agent_status(&mut self, status: AgentStatusSnapshot) {
        let AgentStatusSnapshot {
            agent_id,
            subagent_type,
            turn,
            tokens,
            tool_uses,
            last_activity,
            completed,
        } = status;
        // A running child (no completed status) drives the live
        // refetch below; capture it before the fleet update moves
        // the field.
        let is_running = completed.is_none();
        // Auto-exit the teammate view only when the viewed child is
        // gone or broken (killed/failed). A turn-limit, budget, or
        // normal completion leaves partial output worth reading, so
        // the view stays — the user exits with Shift+Up/Down (Esc
        // only interrupts the viewed child's current turn).
        if self
            .teammate_view
            .as_ref()
            .is_some_and(|v| v.child_sid == agent_id)
            && completed
                .as_deref()
                .is_some_and(|s| matches!(s, "killed" | "failed"))
        {
            self.exit_teammate_view();
        }
        if let Some(entry) = self
            .fleet
            .entries
            .iter_mut()
            .find(|e| e.agent_id == agent_id)
        {
            entry.turn = turn;
            entry.tokens = tokens;
            entry.tool_uses = tool_uses;
            entry.last_activity = last_activity;
            // Stamp the terminal moment the first time a completion
            // lands so the footer grace window starts then; a later
            // status echoing the same completion does not reset it.
            if completed.is_some() && entry.completed.is_none() {
                entry.completed_at = Some(Instant::now());
            }
            entry.completed = completed;
        } else {
            self.fleet.entries.push(FleetEntry {
                agent_id: agent_id.clone(),
                subagent_type,
                turn,
                tokens,
                tool_uses,
                last_activity,
                completed_at: completed.as_ref().map(|_| Instant::now()),
                started_at: Some(Instant::now()),
                completed,
            });
        }
        // Live-tracking: when the user is viewing a running child,
        // each turn-advance Progress refetches the child transcript
        // so the drilled-in view streams the child's turns as they
        // land (not a frozen snapshot taken at enter). The turn
        // guard debounces: one fetch per turn, not one per status
        // echo. A completed child stops refetching (the final fetch
        // on enter already holds the full result).
        if is_running
            && let Some(view) = self.teammate_view.as_mut()
            && view.child_sid == agent_id
            && view.last_fetched_turn.is_none_or(|t| turn > t)
        {
            view.last_fetched_turn = Some(turn);
            if let Some(s) = self.session.as_ref() {
                match s.next_request_id() {
                    Ok(req_id) => {
                        self.enqueue_refresh(ClientCommand::ChildTranscriptQuery {
                            req_id,
                            child_sid: FrontendSessionId(agent_id.clone()),
                        });
                    }
                    Err(_) => self.note_request_id_exhausted(),
                }
            }
        }
    }

    /// Apply a server-initiated request: a reverse ask the user must answer.
    fn apply_server_request(&mut self, request: RequestId, payload: ServerRequest) {
        match payload {
            ServerRequest::Permission { ask } => {
                // Rebuild the transcript from the wire stream so the assistant
                // pre-text + the tool call surface before the user decides.
                // The server blocks on the reverse response, so the run is
                // paused waiting on a human verdict — stop the spinner (busy
                // goes false) and raise the card; resolve_current_approval
                // flips busy back on when the verdict ships. The driver has
                // already shipped every Frame up to this point, so App's own
                // frame log is current.
                self.rebuild_transcript();
                self.raise_agent_approval(*ask, request);
            }
            ServerRequest::Trust { prompt } => {
                // Startup workspace-trust gate: the server blocks before the
                // run loop until the user answers. Raise the trust card (no
                // run to pause — busy is already false at startup, but the
                // card's presence gates new message sends until resolved).
                self.prompt = Some(PendingPrompt::trust_ask(request, prompt));
            }
        }
    }

    /// Apply a reply to a request the client issued. The request id answers
    /// which verb sent it; every response carries it.
    fn apply_response(&mut self, request: RequestId, response: ServerResponse) {
        match response {
            ServerResponse::Done { result } => {
                // Settle only the run this Done answers. A stale or
                // misattributed Done (its request id is not the active
                // run's) must neither clear the active run nor complete
                // it — the same gate the run's Error reply already applies.
                if self.active_run_req_id().is_some_and(|r| r == request) {
                    self.handle_run_completion(result.map_err(|e| e.message));
                }
            }
            ServerResponse::Error { message } => {
                self.apply_response_error(request, message);
            }
            ServerResponse::Ack => {
                // A fire-and-forget request (e.g. SessionReset) came back
                // acknowledged. The host acted locally at send time, so
                // there is nothing to apply — the acknowledgement only
                // closes the request's identity at the event loop.
            }
            ServerResponse::Status { snapshot } => {
                // Cache the snapshot for the status bar + /status pane. NOTE:
                // rename's reply rides this variant (a racing /status is swallowed).
                self.pending_status_command = false;
                // Sync the terminal tab title (OSC 0/2) on change only (not
                // unconditionally every status update).
                sync_terminal_title(&snapshot, &mut self.last_title);
                self.status_cache = Some(*snapshot);
            }
            ServerResponse::Trajectory { entries, redundant } => {
                self.system_line(render_trajectory_wire(&entries, &redundant));
            }
            ServerResponse::Context { breakdown } => {
                self.apply_context_breakdown(breakdown);
            }
            ServerResponse::Compact { reply } => {
                self.apply_compact(reply);
            }
            ServerResponse::PermissionMode { mode } => {
                // Silent update: the status bar reflects the new mode on the
                // next render. No transcript line — mode switching is a
                // background state change, not a conversation event. The
                // /mode command pushes its own feedback when invoked.
                self.mode_cache = Some(mode);
            }
            ServerResponse::PermissionRules { rules } => {
                self.rules_cache = rules.clone();
                self.system_line(render_permission_rules_wire(&rules));
            }
            ServerResponse::PermissionDirs { dirs } => {
                self.dirs_cache = dirs.clone();
            }
            ServerResponse::PermissionAskBeforeGit { enabled } => {
                self.ask_before_git_enabled = enabled;
                self.system_line(format!(
                    "permission: ask before git operations: {} (git commit/rebase/reset/tag {} before running)",
                    if enabled { "on" } else { "off" },
                    if enabled { "ask" } else { "run without asking" },
                ));
            }
            ServerResponse::Tools { tools } => {
                self.tool_entries = tools;
            }
            ServerResponse::Agents { directory } => {
                self.agent_directory = Some(directory);
            }
            ServerResponse::ChildTranscript { child_sid, frames } => {
                self.apply_child_transcript(child_sid, frames);
            }
            ServerResponse::Hooks { hooks } => {
                self.hook_entries = hooks;
            }
            ServerResponse::Skills { skills } => {
                self.skill_entries = skills;
            }
            ServerResponse::SkillBody { body } => {
                self.skills_pane.apply_detail(request, body);
            }
            ServerResponse::Model { result } => {
                self.apply_model_result(request, result);
            }
            ServerResponse::ModelInfo { catalog } => {
                self.model_picker.refresh_snapshot(catalog);
            }
            ServerResponse::MemoryList { entries } => {
                self.apply_memory_list(request, entries);
            }
            ServerResponse::MemoryShow { entry } => {
                self.apply_memory_show(request, entry);
            }
            ServerResponse::MemoryToggleState { state } => {
                self.apply_memory_toggles(request, state);
            }
            ServerResponse::Undo { description } => match description {
                Some(desc) => self.system_line(format!("undo: {desc}")),
                None => self.system_line("undo: nothing to undo (stack empty)"),
            },
            ServerResponse::Debug { state } => {
                if state.enabled {
                    self.system_line(format!("debug: logging to {}", state.path));
                } else {
                    self.system_line("debug: logging off");
                }
            }
        }
    }

    /// Route a per-request protocol error: the active run's own error
    /// resolves its Done; a memory pane mutation's error keeps the action
    /// context; anything else becomes a plain system line.
    fn apply_response_error(&mut self, request: RequestId, message: String) {
        if self.active_run_req_id().is_some_and(|r| r == request) {
            self.handle_run_completion(Err(message));
        } else if self
            .model_picker
            .pending_request
            .as_ref()
            .is_some_and(|p| p.req_id == request)
        {
            self.fail_model_pick(request, &message);
        } else if let Some(action) = self.memory.take_action(request) {
            // A failed pane mutation keeps the action context, so the
            // outcome says what did not happen instead of a bare error.
            // The pending mark clears and the header keeps the old
            // value — nothing was applied.
            self.system_line(Self::memory_failure_line(action, &message));
        } else {
            // An error for the request the skills detail waits on settles it
            // as unavailable rather than leaving it loading on a reply that
            // cannot arrive; any other request id leaves the detail alone.
            self.skills_pane.fail_detail(request);
            self.system_line(format!("error: {message}"));
        }
    }

    /// Render a context-window breakdown as the inline context grid (a
    /// first-class transcript block) rather than a flat one-line system
    /// message, so the proportional grid, legend, and suggestions all
    /// render. The breakdown is cached so the next /context renders
    /// immediately; a fresh ContextQuery refreshes it in the background.
    /// Drill-down (memory files, skills) is empty until the server ships
    /// those sections; the grid itself is honest data from the breakdown.
    fn apply_context_breakdown(&mut self, breakdown: ContextBreakdown) {
        self.context_cache = Some(breakdown.clone());
        let suggestions = suggestions_for(&breakdown);
        let view = ContextView {
            breakdown,
            drill: ContextDrillDown::default(),
            suggestions,
        };
        // Replace the view the newest grid renders, so a refresh updates the
        // grid where it stands instead of stacking a second one. The grid's
        // frame holds its place, so the replacement needs no search by
        // position: whatever rows landed between the cached push and this
        // reply stay below the grid they followed.
        self.replace_context_view(view);
    }

    /// Render the compaction outcome as a one-line system message,
    /// "Compacted ..." / "Not enough messages to compact." wording (no
    /// "compact:" prefix on the outcome — the prefix stays on the guard
    /// errors only). The checkpoint id is internal (a future rewind
    /// handle), kept out of the transcript; the compact count + token
    /// drop are the user-facing outcome.
    fn apply_compact(&mut self, reply: CompactReply) {
        let line = if reply.made_progress {
            let tokens = match (reply.pre_compact_tokens, reply.post_compact_tokens) {
                (Some(pre), Some(post)) => {
                    format!(" · {pre} → {post} estimated tokens")
                }
                _ => String::new(),
            };
            format!("Compacted {} events{}", reply.folded_count, tokens)
        } else {
            "Not enough messages to compact.".to_string()
        };
        self.system_line(line);
    }

    /// Fill a Subagent fold-group with the fetched child transcript,
    /// projected through the same pipeline as the parent flow; the teammate
    /// view swaps too when it shows the same child.
    fn apply_child_transcript(&mut self, child_sid: String, frames: Vec<TranscriptFrame>) {
        // Empty frames mean the child log is missing or produced no durable
        // events; the placeholder line tells a running child (log not yet
        // landed) from a real fetch failure so the error is not hidden
        // behind a "starting" label.
        let folded = if frames.is_empty() {
            vec![self.empty_child_transcript_line(&child_sid)]
        } else {
            // The child log is fetched whole, and the fetch may land while
            // the child is still running: its last turn is left open rather
            // than summarized from frames that are still arriving.
            transcript_from_frames(&frames, 0..frames.len(), true)
        };
        // Swap the child rows into the matching Subagent line in place
        // to preserve position. Mirrors the ContextGrid refresh.
        let idx = self.transcript.lines().iter().rposition(
            |l| matches!(l, TranscriptLine::Subagent { child_sid: c, .. } if c == &child_sid),
        );
        if let Some(idx) = idx {
            let mut line = self.transcript.lines_mut().remove(idx);
            if let TranscriptLine::Subagent {
                folded_transcript, ..
            } = &mut line
            {
                *folded_transcript = folded.clone();
            }
            self.transcript.lines_mut().insert(idx, line);
            // The block holds the frame-derived copy the next rebuild
            // migrates from, so the fetched rows must reach it too, or a
            // rebuild that re-derives this block reads an empty old copy.
            self.transcript
                .blocks_mut()
                .set_subagent_folded(&child_sid, &folded);
            // The swap mutates a line's payload in place instead of
            // pushing, so the row cache needs an explicit bump — the
            // fetched child rows would otherwise stay invisible until
            // an unrelated change invalidated the cache.
            self.bump_transcript_version();
        }
        // When the fetched child is the one the user is viewing, swap
        // the rows into the teammate view too.
        if self
            .teammate_view
            .as_ref()
            .is_some_and(|v| v.child_sid == child_sid)
        {
            self.fill_teammate_view(&child_sid, folded);
        }
    }

    /// Append a durable frame batch and rebuild the transcript once. Batching
    /// keeps reconnect cost linear while still making mid-run frames visible.
    pub(crate) fn apply_frames(&mut self, frames: impl IntoIterator<Item = TranscriptFrame>) {
        let mut any = false;
        let mut assistant_committed = false;
        for frame in frames {
            if let Some(msg) = frame_log_msg(&frame) {
                tracing::debug!(msg);
            }
            assistant_committed |= matches!(
                &frame,
                TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(_))
            );
            // A server user message replaces the tentative echo the frontend
            // raised when it submitted: drop that echo so the row renders once
            // from the server frame. This is the one place that knows the
            // server user message is arriving, so push_frame stays a pure
            // append and the streaming token / tool-update hot path pays
            // nothing for echo bookkeeping.
            if let TranscriptFrame::Session(SessionUpdate::UserMessageChunk(chunk)) = &frame {
                self.transcript.drop_trailing_echo(chunk_text(chunk));
            }
            self.track_running_tool(&frame);
            self.transcript.push_frame(frame);
            any = true;
        }
        if any {
            if assistant_committed && let Some(p) = self.run_progress_mut() {
                p.live_assistant_text.clear();
                p.live_active = false;
            }
            self.rebuild_transcript();
        }
    }

    /// Maintain the running-tools set from a live frame: a ToolCall frame
    /// marks its call id running; a ToolCallUpdate with a terminal status
    /// (completed or failed) finishes it. Non-tool frames are ignored. The set
    /// drives the spinner's tool-use pulse and the stall-gradient exemption.
    /// Finishing a tool also resets the stall clock: last_delta_at is stale
    /// from before the tool ran, and without a fresh grace period the spinner
    /// would snap red the moment the exemption lifts.
    fn track_running_tool(&mut self, frame: &TranscriptFrame) {
        let TranscriptFrame::Session(update) = frame else {
            return;
        };
        match update {
            SessionUpdate::ToolCall(call) => match call.status {
                ToolCallStatus::Completed | ToolCallStatus::Failed => {
                    self.finish_tool(&call.tool_call_id.0);
                }
                _ => {
                    if let Some(p) = self.run_progress_mut() {
                        p.running_tools.insert(call.tool_call_id.0.clone());
                        // A tool is now running: the active streaming block is no
                        // longer reasoning, so the spinner verb must read Working
                        // (not stay Thinking from the last reasoning delta).
                        p.live_block = LiveBlock::Responding;
                    }
                }
            },
            SessionUpdate::ToolCallUpdate(upd)
                if matches!(
                    upd.fields.status,
                    Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
                ) =>
            {
                self.finish_tool(&upd.tool_call_id.0);
            }
            _ => {}
        }
    }

    /// Remove a finished tool call from the running set. On an actual removal the
    /// stall clock resets: last_delta_at is stale from before the tool ran,
    /// and without a fresh grace period the spinner would snap red the moment
    /// the tool-runtime stall exemption lifts.
    fn finish_tool(&mut self, call_id: &str) {
        if let Some(p) = self.run_progress_mut() {
            if p.running_tools.remove(call_id) {
                p.last_delta_at = Some(Instant::now());
            }
            // Drop the elapsed ticker for this call — the authoritative result
            // frame has landed, the chip no longer shows (Ns).
            p.bash_progress.remove(call_id);
        }
    }
}

/// Describe a tool frame for diagnostic logging without recording payload
/// contents. Call frames include identity; result frames include output shape.
fn frame_log_msg(frame: &TranscriptFrame) -> Option<String> {
    let TranscriptFrame::Session(update) = frame else {
        return None;
    };
    match update {
        SessionUpdate::ToolCall(tc) => {
            Some(format!("call id={} tool={}", tc.tool_call_id.0, tc.title))
        }
        SessionUpdate::ToolCallUpdate(upd) => {
            let id = &upd.tool_call_id.0;
            let shape = match upd.fields.raw_output.as_ref() {
                Some(o) => {
                    if o.get("diff").is_some() {
                        "diff"
                    } else if o.get("content").is_some() {
                        "content"
                    } else if o.get("error").is_some() {
                        "error"
                    } else if o.get("files").is_some() || o.get("num_files").is_some() {
                        "files"
                    } else if o.get("stdout").is_some() {
                        "stdout"
                    } else {
                        "other"
                    }
                }
                None => "status",
            };
            Some(format!("result id={id} shape={shape}"))
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "agent_dispatch/tests.rs"]
mod tests;
