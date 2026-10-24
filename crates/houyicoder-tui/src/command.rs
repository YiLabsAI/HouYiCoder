//! Slash-command dispatch and the session-wide commands, as methods on App.
//! Lives here (not in app.rs) so keys.rs can depend on state alone and
//! app.rs can depend on keys + state, giving a clean dependency DAG:
//! keys -> state, command -> state, app -> keys + state. Input submission and
//! the stage chain have their own modules beside this one; the per-pane
//! approval state machine and artifact closed-loop actions live in
//! approval.rs (second impl block).

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::SlashCommand;
use houyicoder_protocol::frontend::status::StatusSnapshot;

use crate::agent_message::ClientCommand;
use crate::composition;
use crate::records::{ContextDrillDown, ContextView, TranscriptLine};
use crate::state::{App, Pane, Screen, Stage};
use crate::view::export_log::write_atomic_0600;
use crate::view::trajectory_pane;
use houyicoder_protocol::llm::Usage;
use std::path::PathBuf;

mod agents;
mod debug;
mod memory;
mod model;
mod resume;
mod skills;
mod status_name;
mod submit;
mod workflow;

mod compact;
mod permission;
pub(crate) mod render;
mod worktree;

impl App {
    /// Execute a slash command. The guided-chain commands (/spec /plan
    /// /implement /review /verify) open their pane and enter the drafting
    /// stage; approval (a) inside the pane advances to the next stage. The
    /// other commands produce visible output (a pane switch or a real
    /// multi-line system entry), not a one-line placeholder. The pane family
    /// lives in open_pane_command.
    pub(crate) fn run_command(&mut self, cmd: SlashCommand) {
        match cmd {
            SlashCommand::Exit => self.ui.quit = true,
            SlashCommand::Login => self.ui.navigation.screen = Screen::Login,
            SlashCommand::Console => self.ui.navigation.screen = Screen::Console,
            SlashCommand::Clear => self.clear_session(),
            SlashCommand::Init => {
                self.enter_stage(Stage::Design, Pane::Spec, "initializing project spec")
            }
            SlashCommand::Spec => {
                self.enter_stage(Stage::Design, Pane::Spec, "drafting design (spec)")
            }
            SlashCommand::Plan => {
                self.enter_stage(Stage::Design, Pane::Plan, "drafting design (plan)")
            }
            SlashCommand::Implement => {
                self.enter_stage(Stage::Implementing, Pane::Diff, "implementing")
            }
            SlashCommand::Review => {
                self.enter_stage(Stage::Verify, Pane::Review, "verifying: agent review")
            }
            SlashCommand::Verify => {
                self.enter_stage(Stage::Verify, Pane::Verify, "verifying: machine check")
            }
            SlashCommand::Graph
            | SlashCommand::Diff
            | SlashCommand::Agents
            | SlashCommand::Context
            | SlashCommand::Status
            | SlashCommand::Trajectory
            | SlashCommand::Tools
            | SlashCommand::Skills
            | SlashCommand::Hooks
            | SlashCommand::Memory => self.open_pane_command(cmd),
            SlashCommand::Model => self.open_model_pane(),
            SlashCommand::Sandbox => self.system_line(render::render_sandbox(
                &self.snapshot_or_stub(),
                &self.sessions.current.sandbox,
            )),
            SlashCommand::Permission => self.open_permission_pane(),
            SlashCommand::Rewind => self.rewind(),
            SlashCommand::Replay => {
                self.workflow.replaying = true;
                self.system_line("replay: entering replay mode (canned indicator on)");
            }
            SlashCommand::ReleaseNotes => self.system_line(composition::release_notes()),
            SlashCommand::Help => self.system_line(composition::help_text()),
            SlashCommand::Worktree => {
                self.ui.navigation.pane = Pane::Worktree;
                self.refresh_worktrees();
            }
            SlashCommand::Compact => self.run_compact(),
            SlashCommand::Undo => {
                let Some(s) = self.sessions.current.connection.as_ref() else {
                    self.system_line("undo: not connected");
                    return;
                };
                let Ok(req_id) = s.next_request_id() else {
                    self.system_line("undo: request ids exhausted");
                    return;
                };
                if let Err(e) = self.enqueue(ClientCommand::UndoQuery { req_id }) {
                    self.system_line(Self::enqueue_failure_line("undo", e));
                } else {
                    self.system_line("undo: reverting most recent destructive op...");
                }
            }
            // Palette-registered local commands. The argless select form (and
            // any direct run_command call) delegates to the string dispatcher;
            // arg-bearing forms typed in the input box are caught there first
            // by submit_input, so the palette reaches it only for the bare name.
            SlashCommand::Search
            | SlashCommand::Export
            | SlashCommand::Resume
            | SlashCommand::Debug => {
                self.run_tui_local_command(cmd.name().trim_start_matches('/'));
            }
        }
    }

    /// Open the pane a command belongs to and, on a connected session, ask the
    /// server to fill it. The pane opens either way: a stub session shows the
    /// empty pane rather than swallowing the command.
    fn open_pane_command(&mut self, cmd: SlashCommand) {
        match cmd {
            SlashCommand::Graph => {
                self.ui.navigation.pane = Pane::Graph;
                self.system_line("graph: showing impact_set result in the graph pane");
            }
            SlashCommand::Diff => {
                self.ui.navigation.pane = Pane::Diff;
                self.system_line("diff: showing changes in the diff pane");
            }
            SlashCommand::Agents => {
                self.ui.navigation.pane = Pane::Agents;
                self.refresh_agents();
                self.refresh_query(|req_id| ClientCommand::AgentsQuery { req_id });
            }
            SlashCommand::Context => {
                // Render from the cache first so the grid appears without a
                // "fetching" placeholder; the query's reply replaces it.
                let status = &self.sessions.current.capabilities.status;
                if let Some(breakdown) = status.context_cache.clone() {
                    let suggestions = composition::suggestions_for(&breakdown);
                    let drill = ContextDrillDown::from_breakdown(&breakdown);
                    let view = ContextView {
                        breakdown,
                        drill,
                        suggestions,
                    };
                    self.push_transcript_line(TranscriptLine::ContextGrid(view));
                } else if self.sessions.current.connection.is_none() {
                    self.push_transcript_line(TranscriptLine::ContextGrid(
                        composition::context_view(),
                    ));
                }
                self.refresh_query(|req_id| ClientCommand::ContextQuery { req_id });
            }
            SlashCommand::Status => {
                // The shared pane template: transcript tail above, status
                // fields below. The pending flag is set only after the enqueue
                // accepts, so a closed driver leaves no flag waiting on a reply
                // that never comes; ConnectionLost announces the loss.
                self.ui.navigation.pane = Pane::Status;
                let Some(s) = self.sessions.current.connection.as_ref() else {
                    return;
                };
                match s.next_request_id() {
                    Ok(req_id) => {
                        let status = &mut self.sessions.current.capabilities.status;
                        if s.enqueue(ClientCommand::StatusQuery { req_id }).is_ok() {
                            status.mark_command_pending();
                        }
                    }
                    Err(_) => self.note_request_id_exhausted(),
                }
            }
            SlashCommand::Trajectory => {
                self.ui.navigation.pane = Pane::Trajectory;
                self.ui.trajectory.set_level(0);
                self.ui.trajectory.set_turn_idx(0);
                self.ui.trajectory.clear_drill();
                // The pane opens on the tail, so a window the last visit
                // walked back from is put back on the newest turns.
                match self.sessions.persistence.trajectory_log.as_ref() {
                    Some(log) => {
                        log.return_to_tail();
                        let view = log.trajectory();
                        // The selection is the newest turn, not the last row
                        // in hand: the page holding that turn may still be
                        // arriving, and the first render would otherwise turn
                        // the open-at-the-tail request into a cursor on the
                        // row that page replaces.
                        trajectory_pane::select_newest_turn(&self.ui.trajectory, &view);
                    }
                    // With no paged history the rows in hand are the whole
                    // list, so the draw can clamp the cursor to its last row.
                    None => self.ui.trajectory.clear_selection(),
                }
                // The cursor starts at the tail; the draw clamps it to the
                // active row count on first render.
                self.ui.trajectory.set_cursor(usize::MAX);
            }
            SlashCommand::Tools => {
                self.ui.navigation.pane = Pane::Tools;
                self.refresh_query(|req_id| ClientCommand::ToolListQuery { req_id });
            }
            SlashCommand::Skills => {
                self.ui.navigation.pane = Pane::Skills;
                self.refresh_query(|req_id| ClientCommand::SkillsQuery { req_id });
            }
            SlashCommand::Hooks => {
                self.ui.navigation.pane = Pane::Hooks;
                self.refresh_query(|req_id| ClientCommand::HooksQuery { req_id });
            }
            SlashCommand::Memory => {
                self.ui.navigation.pane = Pane::Memory;
                self.ui.memory.close_detail();
                self.refresh_query(|req_id| ClientCommand::MemoryListQuery { req_id });
                self.refresh_query(|req_id| ClientCommand::MemoryToggleStateQuery { req_id });
            }
            _ => {}
        }
    }

    /// Issue a refresh query on the live connection, or note that the request
    /// ids ran out. A session with no connection has nothing to ask, and the
    /// pane it opened still shows what it has.
    fn refresh_query(&mut self, query: impl FnOnce(RequestId) -> ClientCommand) {
        let Some(s) = self.sessions.current.connection.as_ref() else {
            return;
        };
        match s.next_request_id() {
            Ok(req_id) => self.enqueue_refresh(query(req_id)),
            Err(_) => self.note_request_id_exhausted(),
        }
    }

    /// /tips + /debug live in the debug submodule.
    /// /export [path]: serialize the durable session trajectory + tool stats +
    /// usage + checkpoints + errors to a JSON file. The implementation builds
    /// the document + a suggested filename from the event stream; an explicit
    /// path overrides the suggestion, otherwise the suggestion lands in the
    /// cwd. Writes are atomic + 0o600 (owner-only) so a half-written export
    /// never appears + the real tool I/O / reasoning inside stays
    /// owner-isolated. Reports the path + byte count so the user knows it
    /// landed without opening the file.
    pub(crate) fn run_export(&mut self, path: Option<&str>) {
        if self.sessions.current.connection.is_none() {
            self.system_line("export: not connected");
            return;
        }
        // Connected but the export source is not installed: a capability gap,
        // not a disconnect.
        let Some(log) = self.sessions.persistence.export_log.as_ref() else {
            self.system_line("export: unavailable in this session");
            return;
        };
        let payload = log.export();
        let target = match path {
            Some(p) => PathBuf::from(p),
            None => PathBuf::from(&payload.filename),
        };
        let bytes = payload.json.as_bytes();
        match write_atomic_0600(&target, bytes) {
            Ok(()) => self.system_line(format!(
                "export: wrote {} ({} bytes)",
                target.display(),
                bytes.len()
            )),
            Err(e) => self.system_line(format!(
                "export: could not write {} ({e})",
                target.display()
            )),
        }
    }

    /// The model label the status bar shows: the applied model the host
    /// reported, from the session's model catalog.
    pub(crate) fn status_bar_model(&self) -> &str {
        &self.sessions.current.capabilities.model.catalog.applied.id
    }

    /// The last observed status snapshot, or None while disconnected or
    /// before the first status reply. The status bar reads the breaker state
    /// and the context gauge through this instead of reaching into the
    /// session's capability nesting.
    pub(crate) fn status_snapshot(&self) -> Option<&StatusSnapshot> {
        self.sessions.current.capabilities.status.snapshot()
    }

    /// The runner's live status snapshot, or a stub built from the no-runner
    /// state (login / console / tests) so /context /status /sandbox still
    /// render a real-shaped layout with zeroed usage instead of a canned
    /// string that hides which fields are live.
    pub(crate) fn snapshot_or_stub(&self) -> StatusSnapshot {
        let status = &self.sessions.current.capabilities.status;
        let status_cache = status.snapshot().cloned();
        status_cache.unwrap_or_else(|| StatusSnapshot {
            model: self
                .sessions
                .current
                .capabilities
                .model
                .catalog
                .applied
                .id
                .clone(),
            breaker_state: None,
            breaker_reason: None,
            breaker_cool_down_secs: None,
            cumulative_usage: Usage::default(),
            last_input_tokens: 0,
            context_window: 0,
            tool_calls: 0,
            tool_success: 0,
            tool_errors: 0,
            descriptor: None,
            ..Default::default()
        })
    }

    /// /clear: archive the transcript, reset the full chain state, and land
    /// back on a fresh working surface (stage Idle, pane Transcript, step idle).
    fn clear_session(&mut self) {
        self.sessions.current.transcript.reset();
        // A read running against the archived history belongs to it: drop the
        // record and move the generation on, so a late result cannot land in
        // the fresh view whose numbers match the old dispatch again.
        self.sessions.current.history_reads.invalidate();
        // The scrollback boundary belongs to the cleared session, so the next
        // one starts at the frame cap instead of an earlier window.
        self.sessions.current.loaded_from_frame.set(usize::MAX);
        let permission = &mut self.sessions.current.capabilities.permission;
        permission.verdict_cursor = 0;
        permission.verdict_log_cache.clear();
        self.sessions.current.todos.clear();
        // The cached breakdown and the occupancy the gauge shows describe the
        // session being archived, not the fresh one. A /context would paint the
        // archived numbers before its reply lands, and the gauge would keep
        // reporting the old footprint until the next poll: drop both at the
        // boundary, and let the fresh session fill them from its own replies.
        let status = &mut self.sessions.current.capabilities.status;
        status.context_cache = None;
        if let Some(snapshot) = status.status_cache.as_mut() {
            snapshot.context_used_tokens = None;
        }
        // Reset the server's cumulative usage tally + audit trajectory so
        // /context reflects the new session only. Fire-and-forget: the host
        // clears its local view in parallel, and a refused enqueue stays
        // quiet because the ConnectionLost event announces the loss.
        if let Some(s) = self.sessions.current.connection.as_ref() {
            match s.next_request_id() {
                Ok(req_id) => {
                    let session_id = self.sessions.current.identity.session_id.clone();
                    let _ = s.enqueue(ClientCommand::SessionReset { req_id, session_id });
                }
                Err(_) => self.note_request_id_exhausted(),
            }
        }
        // The reset clears the server buffer regardless of whether a req_id
        // was issued, so a Message still in the pending copy is now an orphan.
        // Host state invalidation is decoupled from id generation.
        self.ui.input.queue.demote_to_parked();
        self.system_line("session archived, new session started");
        // The history on view is the one being archived, so a viewport parked
        // in it holds a row the fresh session does not have. Release it here,
        // after the rows the pass installs: a park that named a row of the
        // archived log can never resolve again.
        self.release_history_view();
        self.workflow.reset_to(Stage::Idle);
        self.ui.navigation.pane = Pane::Transcript;
        self.reset_chain_state();
        self.ui.sync_viewport_to_stage(Stage::Idle);
    }
}

#[cfg(test)]
mod tests;
