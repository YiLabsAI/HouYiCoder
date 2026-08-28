//! Inbound agent-message dispatch: map each AgentMessage the driver ships to
//! its App state mutation. Extracted from run_control.rs so that file holds
//! only the run lifecycle + poll tick, not the per-message match.

#[path = "agent_dispatch/done.rs"]
mod done;

use houyicoder_protocol::frontend::run::RunError;
use std::time::Instant;

use crate::agent_message::AgentMessage;
use crate::pending_queue::PendingItem;
use crate::records::TranscriptLine;
use crate::state::{App, Pane};
use crate::view::model_pane::row_for_model_id;

impl App {
    /// Apply one inbound agent message: stream a delta, raise a permission ask,
    /// settle a run on Done, or render a query reply. Drained by poll_agent each
    /// tick; the mutation is the side effect.
    /// Dispatch a wire agent message. Done / a run-matching RequestError end
    /// the run: handle_run_done clears busy, rebuilds the transcript, surfaces
    /// the outcome, and records was_final on status so the event loop's idle
    /// drain can gate the queued-message drain (a deferred resume swap drains
    /// any-end; a queued message only when was_final -- a user interrupt must
    /// not auto-send the next queued message).
    pub fn handle_agent_message(&mut self, msg: AgentMessage) {
        match msg {
            AgentMessage::Done { result } => {
                self.active_run_req_id.set(None);
                self.handle_run_done(result);
            }
            AgentMessage::RequestError { req_id, message } => {
                if self.active_run_req_id.get().is_some_and(|r| r == req_id) {
                    self.active_run_req_id.set(None);
                    self.handle_run_done(Err(RunError {
                        kind: "wire".to_string(),
                        message,
                    }));
                } else {
                    self.system_line(format!("error: {message}"));
                }
            }
            other => {
                self.handle_agent_message_inner(other);
            }
        }
    }

    #[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
    fn handle_agent_message_inner(&mut self, msg: AgentMessage) {
        match msg {
            AgentMessage::Frame(frame) => {
                self.apply_frames(std::iter::once(frame));
            }
            AgentMessage::Delta { text } => {
                self.live_assistant_text.push_str(&text);
                // Assistant text is now the active streaming block: the spinner
                // verb must read Working, not Thinking (a sticky "reasoning
                // ever streamed" test would lock it to Thinking for the rest
                // of the turn even while text is streaming).
                self.live_block = crate::state::enums::LiveBlock::Responding;
                self.live_active = true;
                self.last_delta_at = Some(std::time::Instant::now());
                // Do NOT re-pin to the tail per delta: the draw already pins
                // to the new tail when follow_tail is true, and a user who
                // scrolled up to re-read history must stay where they scrolled.
            }
            AgentMessage::ReasoningDelta { text } => {
                if self.live_reasoning_text.is_empty() && !text.is_empty() {
                    self.thinking_started_at = Some(std::time::Instant::now());
                }
                self.live_reasoning_text.push_str(&text);
                // Reasoning is the active streaming block: the spinner verb
                // reads Thinking while this holds (until an assistant-text
                // Delta or a tool start flips it away).
                self.live_block = crate::state::enums::LiveBlock::Thinking;
                self.live_active = true;
                self.last_delta_at = Some(std::time::Instant::now());
            }
            AgentMessage::ToolProgress {
                call_id,
                elapsed_secs,
                lines,
            } => {
                // A long-running tool ticks elapsed (+ optional stdout line
                // count when the backend streams). The chip render reads
                // this map + running_tools to append (Ns) / (Ns · M lines)
                // after 2s. Only meaningful while the call is in flight;
                // retire_tool clears it when the result lands.
                if self.running_tools.contains(&call_id) {
                    self.bash_progress.insert(
                        call_id,
                        crate::state::BashProgress {
                            elapsed_secs,
                            lines,
                        },
                    );
                }
            }
            AgentMessage::QueueConsumed { texts } => {
                // Drop consumed messages from the pending-input copy (FIFO +
                // text match). A consumed message is no longer pending, so the
                // queue view + the run-boundary drain stay accurate — without
                // this, a message injected mid-run would still sit in the
                // pending copy + be spawned again as a follow-up at run end (double).
                for text in texts {
                    if let Some(pos) = self
                        .pending
                        .iter()
                        .position(|it| matches!(it, PendingItem::Message(t) if t == &text))
                    {
                        self.pending.remove(pos);
                    }
                }
            }
            AgentMessage::PermissionAsk { req_id, ask } => {
                // Rebuild the transcript from the wire stream so the assistant
                // pre-text + the tool call surface before the user decides.
                // The server blocks on the reverse response, so the run is
                // paused waiting on a human verdict — stop the spinner (busy
                // goes false) and raise the card; resolve_current_approval
                // flips busy back on when the verdict ships. The driver has
                // already shipped every Frame up to this point, so App's own
                // frame log is current.
                self.rebuild_transcript();
                self.pending_permission_req_id.set(Some(req_id));
                self.raise_agent_approval(ask);
            }
            AgentMessage::TrustAsk { req_id, prompt } => {
                // Startup workspace-trust gate: the server blocks before the
                // run loop until the user answers. Raise the trust card (no
                // run to pause — busy is already false at startup, but the
                // card's presence gates new message sends until resolved).
                self.pending_trust = Some(prompt);
                self.pending_trust_req_id = Some(req_id);
            }
            // Done / RequestError are intercepted by handle_agent_message
            // (which returns was_final); they never reach the inner match.
            AgentMessage::Done { .. } | AgentMessage::RequestError { .. } => {
                unreachable!("run-end variants are intercepted by handle_agent_message")
            }
            AgentMessage::StatusResult { snapshot } => {
                // Cache the snapshot for the status bar + /status pane. NOTE:
                // rename's reply rides this variant (a racing /status is swallowed).
                self.pending_status_command = false;
                // Sync the terminal tab title (OSC 0/2) on change only (not
                // unconditionally every status update).
                crate::terminal_title::sync(&snapshot, &mut self.last_title);
                self.status_cache = Some(snapshot);
            }
            AgentMessage::TrajectoryResult { entries, redundant } => {
                self.system_line(crate::command::render::render_trajectory_wire(
                    &entries, &redundant,
                ));
            }
            AgentMessage::ContextResult { breakdown } => {
                // Cache the breakdown so the next /context renders immediately
                // (no "fetching from server" placeholder); a fresh ContextQuery
                // refreshes it in the background.
                self.context_cache = Some(breakdown.clone());
                // Render the breakdown as the inline context grid (a
                // first-class transcript block) rather than a flat one-line
                // system message, so the proportional grid, legend, and
                // suggestions all render. Drill-down (memory files, skills)
                // is empty until the server ships those sections; the grid
                // itself is honest data from the breakdown.
                let suggestions = crate::composition::suggestions_for(&breakdown);
                let view = crate::records::ContextView {
                    breakdown,
                    drill: crate::records::ContextDrillDown::default(),
                    suggestions,
                };
                // Replace the last ContextGrid (from the /context cache
                // fast-path) so a refresh does not stack two grids. Search
                // backwards instead of checking only the last line: a non-grid
                // line (System, Agent chunk, hook notification) may land
                // between the fast-path push and this reply, and the prior
                // last()-only check would skip the pop → duplicate grid.
                let last_grid = self
                    .transcript
                    .iter()
                    .rposition(|l| matches!(l, TranscriptLine::ContextGrid(_)));
                if let Some(idx) = last_grid {
                    self.transcript.remove(idx);
                }
                self.push_transcript_line(TranscriptLine::ContextGrid(view));
            }
            AgentMessage::CompactResult { reply } => {
                // Render the compaction outcome as a one-line system message,
                // "Compacted ..." / "Not enough
                // messages to compact." wording (no "compact:" prefix on the
                // outcome — the prefix stays on the guard errors only). The
                // checkpoint id is internal (a future rewind handle), kept
                // out of the transcript; the compact count + token drop are
                // the user-facing outcome.
                let line = if reply.made_progress {
                    let tokens = match (reply.pre_compact_tokens, reply.post_compact_tokens) {
                        (Some(pre), Some(post)) => format!(" · {pre} → {post} tokens"),
                        _ => String::new(),
                    };
                    format!("Compacted {} events{}", reply.folded_count, tokens)
                } else {
                    "Not enough messages to compact.".to_string()
                };
                self.system_line(line);
            }
            AgentMessage::PermissionModeResult { mode } => {
                // Silent update: the status bar reflects the new mode on the
                // next render. No transcript line — mode switching is a
                // background state change, not a conversation event. The
                // /mode command pushes its own feedback when invoked.
                self.mode_cache = Some(mode);
            }
            AgentMessage::PermissionRulesResult { rules } => {
                self.rules_cache = rules.clone();
                self.system_line(crate::command::render::render_permission_rules_wire(&rules));
            }
            AgentMessage::PermissionWorkingDirsResult { dirs } => {
                self.dirs_cache = dirs.clone();
            }
            AgentMessage::PermissionAskBeforeGitResult { enabled } => {
                self.ask_before_git_enabled = enabled;
                self.system_line(format!(
                    "permission: ask before git operations: {} (git commit/rebase/reset/tag {} before running)",
                    if enabled { "on" } else { "off" },
                    if enabled { "ask" } else { "run without asking" },
                ));
            }
            AgentMessage::ToolListResult { tools } => {
                self.tool_entries = tools;
            }
            AgentMessage::AgentsResult { directory } => {
                self.agent_directory = Some(directory);
            }
            AgentMessage::ChildTranscriptResult { child_sid, frames } => {
                // Project fetched child frames through the same pipeline as the
                // parent flow. Empty frames mean the child log is missing or
                // produced no durable events; surface an explicit line so the
                // fold-group is non-empty and a re-expand does not refetch.
                let folded = if frames.is_empty() {
                    vec![TranscriptLine::System(
                        "child transcript unavailable".into(),
                    )]
                } else {
                    crate::transcript::transcript_from_frames(&frames)
                };
                // Swap the child rows into the matching Subagent line in place
                // to preserve position. Mirrors the ContextGrid refresh.
                let idx = self
                    .transcript
                    .iter()
                    .rposition(|l| matches!(l, TranscriptLine::Subagent { child_sid: c, .. } if c == &child_sid));
                if let Some(idx) = idx {
                    let mut line = self.transcript.remove(idx);
                    if let TranscriptLine::Subagent {
                        folded_transcript, ..
                    } = &mut line
                    {
                        *folded_transcript = folded.clone();
                    }
                    self.transcript.insert(idx, line);
                }
                // When the fetched child is the one the user is viewing, swap
                // the rows into the teammate view too so the drilled-in
                // transcript fills the same frame the inline fold received.
                // Isomorphic: the view reads the same projection the fold
                // shows, not a parallel simplification.
                if let Some(view) = self.teammate_view.as_mut()
                    && view.child_sid == child_sid
                {
                    view.transcript = folded;
                    self.transcript_scroll.follow_tail = true;
                }
            }
            AgentMessage::HooksResult { hooks } => {
                self.hook_entries = hooks;
            }
            AgentMessage::SkillsResult { skills } => {
                self.skill_entries = skills;
            }
            AgentMessage::ModelResult { model, effort } => {
                self.status.model = model.clone();
                self.model_catalog.active_id = Some(model);
                self.applied_effort = effort;
            }
            AgentMessage::SystemLine { text } => {
                // A runtime notice the agent loop surfaced (e.g. an overflow
                // the catalog could not self-heal). Render verbatim as a
                // transcript system line.
                self.system_line(text);
            }
            AgentMessage::ModelInfoResult { catalog } => {
                self.model_catalog = catalog;
                // Jump the cursor to the active model's row so opening the
                // pane after a switch does not flash from the old position.
                // row_for_model_id owns the +1 for the Default sentinel row;
                // max_sel is catalog.len() (Default + catalog rows - 1).
                let max_sel = self.model_catalog.catalog.len();
                if let Some(ref active) = self.model_catalog.active_id {
                    self.model_sel = row_for_model_id(self, Some(active));
                }
                if self.model_sel > max_sel {
                    self.model_sel = 0;
                }
            }
            AgentMessage::MemoryListResult { entries } => {
                // Populate the memory pane with the real stored-memory list.
                // The wire→pane mapping is a pure fn so it is unit-testable.
                self.memory_entries = crate::command::render::memory_entries_from_wire(&entries);
                // Reset the cursor so it never points past the refreshed list
                // (a forget / rescan shrank it).
                self.memory_list.cursor = 0;
                // Reopen the pane ONLY when the user is still on it. A late
                // list response arriving after the user dismissed the pane
                // must not yank them back — the refresh is for an active
                // viewer, not a stale one. The data still lands (the next
                // /memory open reads it), so nothing is lost.
                if self.pane == Pane::Memory {
                    self.pane = Pane::Memory;
                }
                self.system_line(format!("memory: {} stored", entries.len()));
            }
            AgentMessage::MemoryShowResult { entry } => match entry {
                Some(e) => self.system_line(crate::command::render::render_memory_entry(&e)),
                None => self.system_line("memory: no such key"),
            },
            AgentMessage::MemoryToggleStateResult { state } => {
                // Apply the snapshot (read on pane-open or after a flip) to the
                // toggle view state. Reopen the pane ONLY when the user is
                // still on it — a late flip response arriving after the user
                // dismissed the pane must not yank them back. The toggle still
                // takes effect (state is applied + persisted server-side), so
                // the dismissal is respected without losing the flip.
                self.memory_toggles = state;
                if self.pane == Pane::Memory {
                    self.pane = Pane::Memory;
                }
            }
            AgentMessage::MemorySaved { count, kind } => {
                // A background memory task wrote the given count of entries
                // this pass. Render one notice so the user sees the write
                // without opening the memory pane. The kind names the verb
                // (extract = Saved, dream = Improved); the count gets a
                // singular/plural noun.
                let verb = match kind {
                    houyicoder_protocol::frontend::memory::MemorySavedKind::Extracted => "Saved",
                    houyicoder_protocol::frontend::memory::MemorySavedKind::Consolidated => {
                        "Improved"
                    }
                };
                let plural = if count == 1 { "memory" } else { "memories" };
                self.system_line(format!("{verb} {count} {plural}"));
                // If the memory pane is open, its list is now stale (the
                // background task just wrote). Re-request so the rows refresh.
                if self.pane == Pane::Memory
                    && let Some(req_id) = self.mint_request_id()
                {
                    self.send_cmd(crate::run_control::ClientCommand::MemoryListQuery { req_id });
                }
            }
            AgentMessage::UndoResult { description } => match description {
                Some(desc) => self.system_line(format!("undo: {desc}")),
                None => self.system_line("undo: nothing to undo (stack empty)"),
            },
            AgentMessage::DebugResult { state } => {
                if state.enabled {
                    self.system_line(format!("debug: logging to {}", state.path));
                } else {
                    self.system_line("debug: logging off");
                }
            }
            AgentMessage::AgentStatus {
                agent_id,
                subagent_type,
                turn,
                tokens,
                tool_uses,
                last_activity,
                completed,
            } => {
                // Auto-exit the teammate view only when the viewed child is
                // gone or broken (killed/failed). A turn-limit, budget, or
                // normal completion leaves partial output worth reading, so
                // the view stays — the user exits with Esc.
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
                    self.fleet.entries.push(crate::agent_message::FleetEntry {
                        agent_id,
                        subagent_type,
                        turn,
                        tokens,
                        tool_uses,
                        last_activity,
                        completed_at: completed.as_ref().map(|_| Instant::now()),
                        completed,
                    });
                }
            }
        }
    }

    /// Absorb a run of durable frames into the history and re-project once.
    /// The driver ships one Frame per durable server frame; App owns the
    /// history. Frames must surface as they arrive rather than only at the
    /// next PermissionAsk or Done, since durable frames ship mid-run.
    ///
    /// The projection runs once for the whole run of frames, not once per
    /// frame. No draw happens between two messages of a single drain, so an
    /// intermediate projection is never visible, and the count of frames a
    /// drain hands over is unbounded: a resumed session replays its entire
    /// history in one drain. Re-projecting per frame there costs the frame
    /// count squared and stalls the first paint for minutes on a long
    /// session.
    pub(crate) fn apply_frames(
        &mut self,
        frames: impl IntoIterator<Item = crate::transcript::TranscriptFrame>,
    ) {
        let mut any = false;
        for frame in frames {
            if let Some(msg) = frame_log_msg(&frame) {
                tracing::debug!(msg);
            }
            self.track_running_tool(&frame);
            self.frames.push(frame);
            any = true;
        }
        if any {
            self.rebuild_transcript();
        }
    }

    /// Maintain the running-tools set from a live frame: a ToolCall frame
    /// marks its call id running; a ToolCallUpdate with a terminal status
    /// (completed or failed) retires it. Non-tool frames are ignored. The set
    /// drives the spinner's tool-use pulse and the stall-gradient exemption.
    /// Retiring a tool also resets the stall clock: last_delta_at is stale
    /// from before the tool ran, and without a fresh grace period the spinner
    /// would snap red the moment the exemption lifts.
    fn track_running_tool(&mut self, frame: &crate::transcript::TranscriptFrame) {
        use houyicoder_protocol::frontend::session_update::{SessionUpdate, ToolCallStatus};
        let crate::transcript::TranscriptFrame::Session(update) = frame else {
            return;
        };
        match update {
            SessionUpdate::ToolCall(call) => match call.status {
                ToolCallStatus::Completed | ToolCallStatus::Failed => {
                    self.retire_tool(&call.tool_call_id.0);
                }
                _ => {
                    self.running_tools.insert(call.tool_call_id.0.clone());
                    // A tool is now running: the active streaming block is no
                    // longer reasoning, so the spinner verb must read Working
                    // (not stay Thinking from the last reasoning delta).
                    self.live_block = crate::state::enums::LiveBlock::Responding;
                }
            },
            SessionUpdate::ToolCallUpdate(upd)
                if matches!(
                    upd.fields.status,
                    Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
                ) =>
            {
                self.retire_tool(&upd.tool_call_id.0);
            }
            _ => {}
        }
    }

    /// Retire a tool call from the running set. On an actual removal the
    /// stall clock resets: last_delta_at is stale from before the tool ran,
    /// and without a fresh grace period the spinner would snap red the moment
    /// the tool-runtime stall exemption lifts.
    fn retire_tool(&mut self, call_id: &str) {
        if self.running_tools.remove(call_id) {
            self.last_delta_at = Some(std::time::Instant::now());
        }
        // Drop the elapsed ticker for this call — the authoritative result
        // frame has landed, the chip no longer shows (Ns).
        self.bash_progress.remove(call_id);
    }

    // handle_run_done lives in agent_dispatch/done.rs (file-size split).
}

/// Build a frame-level debug message: the call_id, tool title, and for results
/// the output's shape (diff / content / error / files / stdout / status). Used
/// to capture the exact wire stream a misalignment bug reproduces on so the
/// fix rests on observed data, not inference. The body itself is NOT logged
/// (it can be large + carry file content); the shape tags are enough to spot a
/// swapped call_id at the server or a pairing bug. Pure (no I/O) so it is
/// unit-testable without the debug-log file env.
fn frame_log_msg(frame: &crate::transcript::TranscriptFrame) -> Option<String> {
    use houyicoder_protocol::frontend::session_update::SessionUpdate;
    let crate::transcript::TranscriptFrame::Session(update) = frame else {
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
#[path = "agent_dispatch_tests.rs"]
mod tests;
