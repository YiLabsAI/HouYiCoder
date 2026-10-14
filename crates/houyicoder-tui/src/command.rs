//! Slash-command dispatch and working-surface input parsing, as methods on
//! App. Lives here (not in app.rs) so keys.rs can depend on state alone and
//! app.rs can depend on keys + state, giving a clean dependency DAG:
//! keys -> state, command -> state, app -> keys + state. The per-pane
//! approval state machine and artifact closed-loop actions live in
//! approval.rs (second impl block).

use std::time::Instant;

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::SlashCommand;
use houyicoder_protocol::frontend::status::StatusSnapshot;

use crate::composition;
use crate::input::queue::{InputItem, is_state_changing};
use crate::records::{ContextDrillDown, ContextView};
use crate::run_control::ClientCommand;
use crate::state::{App, ArtifactSession, Pane, Screen, Stage, TranscriptLine, pane_for_stage};
use crate::view::export_log::write_atomic_0600;

mod debug;
mod memory;
mod model;
mod resume;
mod skills;
mod status_name;

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
                    let view = ContextView {
                        breakdown,
                        drill: ContextDrillDown::default(),
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
                // No turn is selected yet: the pane opens on the tail and
                // follows it until the user picks a row.
                self.ui.trajectory.clear_selection();
                self.ui.trajectory.clear_drill();
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
            Some(p) => std::path::PathBuf::from(p),
            None => std::path::PathBuf::from(&payload.filename),
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

    /// Enter a guided stage: push the old stage onto the history, set the new
    /// stage + pane, update the spec strip step, and log the transition.
    pub(crate) fn enter_stage(&mut self, stage: Stage, pane: Pane, msg: &str) {
        self.set_stage(stage);
        self.ui.navigation.pane = pane;
        self.system_line(msg.to_string());
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
            cumulative_usage: houyicoder_protocol::llm::Usage::default(),
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
        self.workflow.reset_to(Stage::Idle);
        self.ui.navigation.pane = Pane::Transcript;
        self.reset_chain_state();
        self.ui.sync_viewport_to_stage(Stage::Idle);
    }

    /// /rewind: pop the last stage transition, land on the matching pane, and
    /// un-approve the artifact of the restored stage so it can be re-drafted.
    fn rewind(&mut self) {
        match self.rewind_stage() {
            Some(prev) => {
                self.ui.navigation.pane = pane_for_stage(prev);
                self.unapprove_artifact(prev);
                self.system_line(format!(
                    "rewound to {} (artifact un-approved)",
                    prev.label()
                ));
            }
            None => self.system_line("rewind: no earlier stage to return to"),
        }
    }

    /// /rewind <stage_name>: rewind to a named stage (spec, plan, implement,
    /// review, verify). Pops history until the target is on top, un-approves
    /// its artifact, and lands on the matching pane. An unknown name reports
    /// an error and returns (no fallback to a one-step rewind — a typo must
    /// not silently rewind). The /memory sub-command (toggle / forget / show)
    /// lives in command_memory so this file stays under the size gate.
    fn rewind_targeted(&mut self, name: &str) {
        if name.is_empty() {
            self.rewind();
            return;
        }
        let Some(target) = parse_stage_name(name) else {
            self.system_line(format!("rewind: unknown stage {name}"));
            return;
        };
        // Pop history until the current stage matches the target (or history
        // runs out), then ensure the target is active.
        while self.workflow.stage != target {
            if self.rewind_stage().is_none() {
                break;
            }
        }
        if self.workflow.stage != target {
            self.set_stage(target);
        }
        self.ui.navigation.pane = pane_for_stage(target);
        self.unapprove_artifact(target);
        self.system_line(format!(
            "rewound to {} (artifact un-approved)",
            target.label()
        ));
    }

    /// Un-approve the artifact associated with a stage so returning to it
    /// forces a fresh approval. Spec and plan have explicit approved flags;
    /// later stages have no single artifact flag so their work is reset by the
    /// chain reset path instead.
    fn unapprove_artifact(&mut self, stage: Stage) {
        if stage == Stage::Design {
            self.workflow.spec_artifact.approved = false;
            self.workflow.plan_artifact.approved = false;
        }
    }

    /// /compact: dispatch a manual compaction over the wire. Guarded against
    /// Route a pasted token to the active input surface: the palette query when
    /// the palette is open (so pasting an argument into the hint-after-space
    /// popup lands in the query, not the input bar), else the input box. The
    /// palette query is the arg surface once a command is selected; routing a
    /// paste there lets the user paste a file path / sid into the popup.
    pub(crate) fn apply_paste_token(&mut self, token: &str) {
        if self.ui.navigation.palette.open {
            self.ui.navigation.palette.query.push_str(token);
        } else {
            self.ui.input.editor.insert_str(token);
        }
    }

    /// Ask the input owner whether the pending paste has elapsed its 50ms gap.
    /// Input owns the accumulator (buffer plus last-chunk time) and its group
    /// consumption; App keeps only the token routing (palette query vs input
    /// box), so a produced token is applied through apply_paste_token. Returns
    /// whether a paste was ingested.
    pub(crate) fn flush_pending_paste(&mut self) -> bool {
        if let Some(token) = self.ui.input.drain_pending_paste(Instant::now()) {
            self.apply_paste_token(&token);
            true
        } else {
            false
        }
    }

    /// Submit the working-surface input. In an artifact edit mode (Replace,
    /// Insert, NaturalLanguage), Enter submits the in-progress edit. Otherwise
    /// parse a leading slash to run a command; in the artifact pane Normal
    /// mode, plain (non-slash) text is a no-op (the input box is for slash
    /// commands; edits start with c/o/d/i). Outside the artifact pane, a typed
    /// task auto-enters the design stage.
    pub(crate) fn submit_input(&mut self) {
        // Artifact edit mode: Enter submits the edit, not a slash command.
        if self.ui.navigation.pane == Pane::Artifact && !self.workflow.artifact.mode().is_normal() {
            let text = self.ui.input.editor.take();
            self.artifact_submit_edit(text);
            return;
        }
        // /permissions typed sub-mode (add / remove / search): Enter submits the
        // pane's typed text, not a chat turn or slash command.
        if self.ui.navigation.pane == Pane::Permission && self.ui.permission_pane.input.is_active()
        {
            let text = self.ui.input.editor.take();
            crate::permission_input::submit_permission_input(self, text);
            return;
        }
        // A fresh submission means the user re-engaged the conversation: jump
        // back to the tail so new content streams in under their attention,
        // clearing any new-messages indicator left from reading history.
        self.scroll_transcript_follow_tail();
        let text = self.ui.input.editor.take();
        if text.is_empty() {
            return;
        }
        if let Some(stripped) = text.strip_prefix('/') {
            // A state-changing command (resume/clear/rewind/undo) submitted
            // while a run is in flight is deferred onto the pending queue
            // (drained FIFO at idle) so it does not fight the in-flight run's
            // writes. A bare /resume (no arg) is NOT deferred -- it opens the
            // picker, which is read-only browsing. UI-local commands run
            // immediately even mid-run.
            let cmd = stripped.split_whitespace().next().unwrap_or("");
            let bare_resume = cmd == "resume" && stripped.trim() == cmd;
            if self.agent_busy() && is_state_changing(stripped) && !bare_resume {
                self.ui.input.queue.push(InputItem::Command(text.clone()));
                self.system_line(self.deferred_command_message(stripped));
                return;
            }
            // Rule: a leading slash short-circuits ONLY
            // when the first token matches a known command. Otherwise the
            // whole input (a path like /home/you/sample-project, or an
            // unknown token like /nope) is a message to the model — never an
            // "unknown command" error. Raise a tentative echo for the
            // command path; if no command matched, pop it so the message path
            // below echoes + sends exactly once (no double echo).
            self.push_unanswered_echo(text.clone());
            if self.run_tui_local_command(stripped.trim()) {
                return;
            }
            if let Some(cmd) = SlashCommand::parse(&text) {
                self.run_command(cmd);
                return;
            }
            // No command matched: undo the tentative echo and fall through to
            // send the /-prefixed input as a message to the model.
            self.drop_tentative_echo();
        }
        // Artifact Normal mode: plain text is not auto-annotated. The input box
        // is for slash commands; direct edits start with c/o/d/i. Drop the text
        // with a hint so the user knows why nothing happened.
        if self.ui.navigation.pane == Pane::Artifact {
            self.system_line("artifact: use c/o/d/i to edit, / for commands");
            return;
        }
        // Auto-start path. When a real runner is wired, spawn runner.run on
        // the tokio runtime; the transcript is rebuilt from real SessionLogEntries
        // when the run lands. An app with no runner reaches no model, so no frame
        // will follow this submission: the frontend raises the row, and the typed
        // task opens the drafting surface it would open with a runner. No reply
        // follows, because there is nothing to reply with.
        let text = crate::paste::PasteStore::expand(&text, &self.ui.input.pasted);
        if self.sessions.current.connection.is_some() {
            let project = crate::history::current_project();
            let session_id = self.sessions.current.identity.session_id.to_string();
            self.ui.input.history.submit(&text, &project, &session_id);
            self.spawn_run(text);
            return;
        }
        self.push_unanswered_echo(text);
        self.enter_stage(
            Stage::Design,
            Pane::Spec,
            "task received -> drafting design",
        );
    }

    /// Parse and run a TUI-local slash command: the arg dispatcher for the
    /// commands that live entirely in the TUI, which the palette's argless
    /// select path cannot carry. Returns true when the input matched; false to
    /// fall through to SlashCommand::parse. The argless palette-select form
    /// delegates here from run_command; the arg-bearing forms typed in the
    /// input box reach here first from submit_input.
    pub(crate) fn run_tui_local_command(&mut self, name: &str) -> bool {
        // /resume (no arg): open the session picker. /resume <id|name>:
        // direct switch to that session. Word-boundary so /resumefoo is not
        // swallowed.
        if name == "resume" || name.starts_with("resume ") {
            let arg = name.strip_prefix("resume ").map(str::trim);
            self.run_resume(arg);
            return true;
        }
        if self.run_permission_command(name) {
            return true;
        }
        // /search <query>: word-boundary match, so /searchalot falls through
        // to the model as a message. Enters the full-screen verbose search
        // view. --all says so rather than silently searching less than asked.
        if name == "search" || name.starts_with("search ") {
            let rest = name.strip_prefix("search ").unwrap_or("").trim();
            let (all, query) = match rest.strip_prefix("--all").map(str::trim_start) {
                Some(q) => (true, q.trim()),
                None => (false, rest),
            };
            if !query.is_empty() {
                if all {
                    self.system_line(
                        "search: full-history search is not in the new view yet; \
                         searching the in-memory window only",
                    );
                }
                self.enter_search_view(query);
                return true;
            }
            self.system_line("search: usage /search KEYWORD");
            return true;
        }
        // /export [path]: serialize the durable session trajectory to a JSON
        // file (the ExPeL self-evolution data source). TUI-local — no server
        // round-trip. Argless writes to a default filename in the cwd
        // ({timestamp}-{first_prompt_slug}.json); an explicit path overrides.
        // The whitespace guard keeps /exportfoo from matching.
        if name == "export" || name.starts_with("export ") {
            let arg = name.strip_prefix("export").unwrap_or("").trim();
            let path = if arg.is_empty() { None } else { Some(arg) };
            self.run_export(path);
            return true;
        }
        // /debug [path|off]: toggle runtime debug logging without restarting.
        if name == "debug" || name.starts_with("debug ") {
            let arg = name.strip_prefix("debug").unwrap_or("").trim();
            self.run_debug(arg);
            return true;
        }
        // /rewind [stage]: word-boundary match so /rewindfoo is NOT swallowed
        // as /rewind foo (unknown stage). argless /rewind rewinds one step.
        if name == "rewind" || name.starts_with("rewind ") {
            let arg = name.strip_prefix("rewind ").unwrap_or("").trim();
            self.rewind_targeted(arg);
            return true;
        }
        // /memory <key>: fetch the full body of one memory (the show path). The
        // argless /memory form is NOT handled here — it falls through to
        // SlashCommand::Memory, which lists. The whitespace guard keeps
        // /memorylane from matching. The /memory toggle <which> sub-command is
        // intercepted first so "toggle" is not read as a memory key.
        if let Some(arg) = name.strip_prefix("memory")
            && let Some(rest) = arg.strip_prefix(|c: char| c.is_whitespace())
            && !rest.trim().is_empty()
        {
            return self.run_memory_subcommand(rest.trim());
        }
        if self.run_artifact_command(name) {
            return true;
        }
        false
    }

    /// /artifact [path] and /artifact-save [path]: the document commands.
    /// /artifact opens the inline-review pane, loading the given path or the
    /// canned stub when none is given; /artifact-save writes the edited
    /// document back. Both are TUI-local, and neither is in the protocol
    /// palette, so they only arrive here as typed input.
    fn run_artifact_command(&mut self, name: &str) -> bool {
        // /artifact-save [path]: an explicit path wins; with none, the
        // document is written back to the file it was loaded from, and the
        // stub has none to write to. No auto-save: explicit only.
        if name == "artifact-save" || name.starts_with("artifact-save ") {
            if self.ui.navigation.pane != Pane::Artifact {
                self.system_line("artifact-save: open an artifact first (/artifact <path>)");
                return true;
            }
            let arg = name.strip_prefix("artifact-save").unwrap_or("").trim();
            let path = if arg.is_empty() {
                match self.workflow.artifact.file_path() {
                    Some(loaded) => loaded.to_string(),
                    None => {
                        self.system_line("artifact-save: pass a path; the stub has no file");
                        return true;
                    }
                }
            } else {
                arg.to_string()
            };
            let lines = self.workflow.artifact.current_lines().len();
            match self.workflow.artifact.save(&path) {
                Ok(()) => self.system_line(format!("artifact: saved {path} ({lines} lines)")),
                Err(_) => self.system_line(format!("artifact: could not write {path}")),
            }
            return true;
        }
        // /artifact [path]: a path loads that document; no path opens the
        // canned stub, which is not a file, so the pane names it as a stub
        // rather than a path nothing backs. Always opens in Working mode:
        // artifact editing needs the input box, and Focus mode hides it
        // (which would dead-end the c/o/d keys).
        if name == "artifact" || name.starts_with("artifact ") {
            let arg = name.strip_prefix("artifact").unwrap_or("").trim();
            if arg.is_empty() {
                self.workflow.artifact = ArtifactSession::stub();
                self.ui.navigation.pane = Pane::Artifact;
                self.system_line("artifact: canned stub (pass a path to open a file)");
            } else {
                match ArtifactSession::load(arg) {
                    Ok(session) => {
                        self.workflow.artifact = session;
                        self.ui.navigation.pane = Pane::Artifact;
                        self.system_line(format!(
                            "artifact: opened {} (c=replace o=insert d=delete i=nl; a/r=review)",
                            self.workflow.artifact.path()
                        ));
                    }
                    Err(_) => {
                        self.ui.navigation.pane = Pane::Artifact;
                        self.system_line(format!(
                            "artifact: could not read {arg}, showing canned stub"
                        ));
                    }
                }
            }
            self.fold_to_working();
            return true;
        }
        false
    }
}

/// Parse a stage name for /rewind <stage_name>. Accepts the short labels and
/// common synonyms. Returns None for an unrecognized name.
fn parse_stage_name(name: &str) -> Option<Stage> {
    match name.trim().to_ascii_lowercase().as_str() {
        "idle" => Some(Stage::Idle),
        "design" | "spec" | "plan" => Some(Stage::Design),
        "implement" | "implementing" => Some(Stage::Implementing),
        "verify" | "review" | "verifying" | "reviewing" => Some(Stage::Verify),
        "done" => Some(Stage::Done),
        _ => None,
    }
}
