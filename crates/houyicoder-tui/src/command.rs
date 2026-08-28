//! Slash-command dispatch and working-surface input parsing, as methods on
//! App. Lives here (not in app.rs) so keys.rs can depend on state alone and
//! app.rs can depend on keys + state, giving a clean dependency DAG:
//! keys -> state, command -> state, app -> keys + state. The per-pane
//! approval state machine and artifact closed-loop actions live in
//! approval.rs (second impl block).

use houyicoder_protocol::frontend::SlashCommand;
use houyicoder_protocol::frontend::status::StatusSnapshot;

use crate::composition;
use crate::pending_queue::{PendingItem, is_state_changing};
use crate::state::{App, ArtifactSession, Pane, Screen, Stage, TranscriptLine, pane_for_stage};
use crate::view::model_pane::row_for_model_id;

/// /memory sub-command + pane-action methods (toggle / forget / cursor),
/// split out so this file stays under the file-size gate.
mod debug;
mod memory;
mod model;
mod resume;
mod status_name;

mod compact;
mod permission;
pub(crate) mod render;
mod worktree;

/// The default document the artifact pane opens when no path is given: the
/// strategy draft itself, so the pane opens on a real multi-section document.
const DEFAULT_ARTIFACT_PATH: &str = "docs/loop-artifacts/00-ten-pillars.md";

impl App {
    /// Execute a slash command. The guided-chain commands (/spec /plan
    /// /implement /review /verify) open their pane and enter the drafting
    /// stage; approval (a) inside the pane advances to the next stage. The
    /// other commands produce visible output (a pane switch or a real
    /// multi-line system entry), not a one-line placeholder.
    #[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
    pub(crate) fn run_command(&mut self, cmd: SlashCommand) {
        use SlashCommand as C;
        match cmd {
            C::Exit => self.quit = true,
            C::Login => self.screen = Screen::Login,
            C::Console => self.screen = Screen::Console,
            C::Clear => self.clear_session(),
            C::Init => self.enter_stage(Stage::Design, Pane::Spec, "initializing project spec"),
            C::Spec => self.enter_stage(Stage::Design, Pane::Spec, "drafting design (spec)"),
            C::Plan => self.enter_stage(Stage::Design, Pane::Plan, "drafting design (plan)"),
            C::Implement => self.enter_stage(Stage::Implementing, Pane::Diff, "implementing"),
            C::Review => self.enter_stage(Stage::Verify, Pane::Review, "verifying: agent review"),
            C::Verify => self.enter_stage(Stage::Verify, Pane::Verify, "verifying: machine check"),
            C::Graph => {
                self.pane = Pane::Graph;
                self.system_line("graph: showing impact_set result in the graph pane");
            }
            C::Diff => {
                self.pane = Pane::Diff;
                self.system_line("diff: showing changes in the diff pane");
            }
            C::Agents => {
                self.pane = Pane::Agents;
                if let Some(req_id) = self.mint_request_id() {
                    self.send_cmd(crate::run_control::ClientCommand::AgentsQuery { req_id });
                }
            }
            C::Model => {
                self.pane = Pane::Model;
                // Position the cursor on the active model's row immediately
                // (from the cached catalog) so the first render shows the
                // correct row — no flicker from the old position waiting
                // for the ModelInfoResult reply. If the catalog is empty
                // (first open), the reply will position it; if stale, the
                // reply corrects it.
                if let Some(ref active) = self.model_catalog.active_id {
                    self.model_sel = row_for_model_id(self, Some(active));
                }
                if let Some(req_id) = self.mint_request_id() {
                    self.send_cmd(crate::run_control::ClientCommand::ModelInfoQuery { req_id });
                }
            }
            C::Sandbox => self.system_line(render::render_sandbox(
                &self.snapshot_or_stub(),
                &self.status.sandbox,
            )),
            C::Permission => self.open_permission_pane(),
            C::Context => {
                // Render immediately from the cache (no "fetching from
                // server" placeholder); a background ContextQuery refreshes
                // it — the ContextResult replaces this grid with fresh data.
                // No cache yet (first /context on a fresh session): the
                // server's prospective breakdown fills it on the first reply.
                if let Some(breakdown) = self.context_cache.clone() {
                    let suggestions = crate::composition::suggestions_for(&breakdown);
                    let view = crate::records::ContextView {
                        breakdown,
                        drill: crate::records::ContextDrillDown::default(),
                        suggestions,
                    };
                    self.push_transcript_line(TranscriptLine::ContextGrid(view));
                } else if self.session.is_none() {
                    self.push_transcript_line(TranscriptLine::ContextGrid(
                        composition::context_view(),
                    ));
                }
                if let Some(req_id) = self.mint_request_id() {
                    self.send_cmd(crate::run_control::ClientCommand::ContextQuery { req_id });
                }
            }
            C::Status => {
                // Open the Status pane (the shared Pane template: transcript
                // tail above, status fields below). A wired session also
                // requests a fresh status poll so the cache is current on
                // open; the reply updates the cache + the pane re-renders,
                // not a transcript text dump. The stub path renders from
                // snapshot_or_stub the pane content reads.
                self.pane = Pane::Status;
                if self.session.is_some() {
                    self.pending_status_command = true;
                    if let Some(s) = self.session.as_ref() {
                        s.request_status();
                    }
                }
            }
            C::Trajectory => {
                self.pane = Pane::Trajectory;
            }
            C::Tools => {
                self.pane = Pane::Tools;
                if let Some(req_id) = self.mint_request_id() {
                    self.send_cmd(crate::run_control::ClientCommand::ToolListQuery { req_id });
                }
            }
            C::Skills => {
                self.pane = Pane::Skills;
                if let Some(req_id) = self.mint_request_id() {
                    self.send_cmd(crate::run_control::ClientCommand::SkillsQuery { req_id });
                }
            }
            C::Hooks => {
                self.pane = Pane::Hooks;
                if let Some(req_id) = self.mint_request_id() {
                    self.send_cmd(crate::run_control::ClientCommand::HooksQuery { req_id });
                }
            }
            C::Memory => {
                // Open the memory pane (the list surface) + refresh from the
                // server. The pane re-renders from memory_entries when the
                // MemoryListResult lands; until then it shows the prior/empty
                // list (no "fetching" row needed — the pane itself is the
                // surface, mirroring /context's cache-then-refresh). The toggle
                // rows read their state in the same open so both rows render.
                self.pane = Pane::Memory;
                if let Some(req_id) = self.mint_request_id() {
                    self.send_cmd(crate::run_control::ClientCommand::MemoryListQuery { req_id });
                }
                if let Some(req_id) = self.mint_request_id() {
                    self.send_cmd(crate::run_control::ClientCommand::MemoryToggleStateQuery {
                        req_id,
                    });
                }
            }
            C::Rewind => self.rewind(),
            C::Replay => {
                self.replaying = true;
                self.system_line("replay: entering replay mode (canned indicator on)");
            }
            C::ReleaseNotes => self.system_line(composition::release_notes()),
            C::Help => self.system_line(composition::help_text()),
            C::Worktree => {
                self.pane = Pane::Worktree;
                self.refresh_worktrees();
            }
            C::Compact => self.run_compact(),
            C::Undo => {
                if let Some(req_id) = self.mint_request_id() {
                    self.send_cmd(crate::run_control::ClientCommand::UndoQuery { req_id });
                    self.system_line("undo: reverting most recent destructive op...");
                } else {
                    self.system_line("undo: no server connected");
                }
            }
            // Palette-registered local commands. The argless select form (and
            // any direct run_command call) delegates to the string dispatcher;
            // arg-bearing forms typed in the input box are caught there first
            // by submit_input, so this arm only fires on the argless path.
            C::Search | C::Export | C::Resume | C::Debug => {
                self.run_tui_local_command(cmd.name().trim_start_matches('/'));
            }
        }
    }

    /// /tips + /debug live in the debug submodule.
    /// /export [path]: serialize the durable session trajectory + tool stats +
    /// usage + checkpoints + errors to a JSON file. The bridge builds the
    /// document + a suggested filename from the event stream; an explicit
    /// path overrides the suggestion, otherwise the suggestion lands in the
    /// cwd. Writes are atomic + 0o600 (owner-only) so a half-written export
    /// never appears + the real tool I/O / reasoning inside stays
    /// owner-isolated. Reports the path + byte count so the user knows it
    /// landed without opening the file.
    pub(crate) fn run_export(&mut self, path: Option<&str>) {
        use crate::view::export_log::write_atomic_0600;
        let Some(log) = self.export_log.as_ref() else {
            self.system_line("export: no session log wired (stub mode)");
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
        self.pane = pane;
        self.system_line(msg.to_string());
    }

    /// The runner's live status snapshot, or a stub built from the no-runner
    /// state (login / console / tests) so /context /status /sandbox still
    /// render a real-shaped layout with zeroed usage instead of a canned
    /// string that hides which fields are live.
    /// The last wire status snapshot cached from the periodic poll, or a
    /// zeroed stub built from the no-runner state (login / console / tests)
    /// so /context /status /sandbox still render a real-shaped layout with
    /// zeroed usage instead of a canned string that hides which fields are
    /// live.
    pub(crate) fn snapshot_or_stub(&self) -> StatusSnapshot {
        self.status_cache.clone().unwrap_or_else(|| StatusSnapshot {
            model: self.status.model.clone(),
            breaker_state: None,
            breaker_reason: None,
            breaker_cool_down_secs: None,
            cumulative_usage: houyicoder_protocol::llm::Usage::default(),
            last_input_tokens: 0,
            context_window: 0,
            tool_calls: 0,
            tool_success: 0,
            tool_errors: 0,
            meta: None,
            ..Default::default()
        })
    }

    /// /clear: archive the transcript, reset the full chain state, and land
    /// back on a fresh working surface (stage Idle, pane Transcript, step idle).
    fn clear_session(&mut self) {
        self.transcript.clear();
        self.frames.clear();
        // Reset the rebuild seal + verdict cursor too — otherwise the next
        // rebuild's incremental path would index a stale prefix (today masked
        // by the need_full rewind check, but make it explicit so a future
        // batch-replay path can't trip a silent duplicate).
        self.sealed_frames_end = 0;
        self.sealed_transcript_len = 0;
        self.verdict_cursor = 0;
        self.verdict_log_cache.clear();
        // Reset the server's cumulative usage tally + audit trajectory so
        // /context reflects the new session only. Fire-and-forget over the
        // wire; the host clears its local view in parallel.
        if let Some(req_id) = self.mint_request_id() {
            let session_id = self.session_id.clone();
            self.send_cmd(crate::run_control::ClientCommand::SessionReset { req_id, session_id });
        }
        // The reset clears the server buffer regardless of whether a req_id
        // was minted, so a Message still in the pending copy is now an orphan.
        // Host state invalidation is decoupled from id-minting.
        self.demote_pending_to_parked();
        self.system_line("session archived, new session started");
        self.stage = Stage::Idle;
        self.spec_ctx.step = Stage::Idle.label().to_string();
        self.pane = Pane::Transcript;
        self.reset_chain_state();
        self.sync_viewport_to_stage();
    }

    /// /rewind: pop the last stage transition, land on the matching pane, and
    /// un-approve the artifact of the restored stage so it can be re-drafted.
    fn rewind(&mut self) {
        match self.rewind_stage() {
            Some(prev) => {
                self.pane = pane_for_stage(prev);
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
        while self.stage != target {
            if self.rewind_stage().is_none() {
                break;
            }
        }
        if self.stage != target {
            self.set_stage(target);
        }
        self.pane = pane_for_stage(target);
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
            self.spec_artifact.approved = false;
            self.plan_artifact.approved = false;
        }
    }

    /// /compact: dispatch a manual compaction over the wire. Guarded against
    /// Route a pasted token to the active input surface: the palette query when
    /// the palette is open (so pasting an argument into the hint-after-space
    /// popup lands in the query, not the input bar), else the input box. The
    /// palette query is the arg surface once a command is selected; routing a
    /// paste there lets the user paste a file path / sid into the popup.
    pub(crate) fn apply_paste_token(&mut self, token: &str) {
        if self.palette.open {
            self.palette.query.push_str(token);
        } else {
            self.input.insert_str(token);
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
        if self.pane == Pane::Artifact && !self.artifact.mode().is_normal() {
            let text = self.input.take();
            self.artifact_submit_edit(text);
            return;
        }
        // /permissions typed sub-mode (add / remove / search): Enter submits the
        // pane's typed text, not a chat turn or slash command.
        if self.pane == Pane::Permission && self.permission_input.is_active() {
            let text = self.input.take();
            crate::permission_input::submit_permission_input(self, text);
            return;
        }
        // A fresh submission means the user re-engaged the conversation: jump
        // back to the tail so new content streams in under their attention,
        // clearing any new-messages indicator left from reading history.
        self.scroll_transcript_follow_tail();
        let text = self.input.take();
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
            if self.agent_busy && is_state_changing(stripped) && !bare_resume {
                self.pending.push(PendingItem::Command(text.clone()));
                self.system_line(self.deferred_command_message(stripped));
                return;
            }
            // Rule: a leading slash short-circuits ONLY
            // when the first token matches a known command. Otherwise the
            // whole input (a path like /home/you/sample-project, or an
            // unknown token like /nope) is a message to the model — never an
            // "unknown command" error. Push a tentative User echo for the
            // command path; if no command matched, pop it so the message path
            // below echoes + sends exactly once (no double echo).
            self.push_transcript_line(TranscriptLine::User(text.clone()));
            if self.run_tui_local_command(stripped.trim()) {
                return;
            }
            if let Some(cmd) = SlashCommand::parse(&text) {
                self.run_command(cmd);
                return;
            }
            // No command matched: undo the tentative echo and fall through to
            // send the /-prefixed input as a message to the model.
            if matches!(self.transcript.last(), Some(TranscriptLine::User(_))) {
                self.transcript.pop();
            }
        }
        // Artifact Normal mode: plain text is not auto-annotated. The input box
        // is for slash commands; direct edits start with c/o/d/i. Drop the text
        // with a hint so the user knows why nothing happened.
        if self.pane == Pane::Artifact {
            self.system_line("artifact: use c/o/d/i to edit, / for commands");
            return;
        }
        // Auto-start path. When a real runner is wired, spawn runner.run on
        // the tokio runtime; the transcript is rebuilt from real TurnEvents
        // when the run lands. Without a runner, fall back to the legacy stub
        // reply so tests and the no-runtime path keep working.
        let text = crate::paste::PasteStore::expand(&text, &self.pasted);
        if self.session.is_some() {
            self.spawn_run(text);
            return;
        }
        self.push_transcript_line(TranscriptLine::User(text));
        self.push_transcript_line(TranscriptLine::Agent(
            "reading files and drafting a design from your task".into(),
        ));
        self.push_transcript_line(TranscriptLine::Read {
            path: "src/lib.rs".to_string(),
        });
        self.enter_stage(
            Stage::Design,
            Pane::Spec,
            "task received -> drafting design",
        );
    }

    /// Parse and run a TUI-local slash command. These commands live entirely
    /// in the TUI (no server round-trip). Search is registered in
    /// the SlashCommand palette (discoverable when the user types /); the
    /// /permissions subcommands (add / del / list / view / git) and the
    /// /artifact + /artifact-save document commands stay string-only on
    /// purpose — arg-bearing forms that the palette's argless select path
    /// cannot carry. This function is the arg
    /// dispatcher: it parses the full input string (including arguments and
    /// subcommands) that the palette's argless select path cannot carry.
    /// Returns true when the input matched a local command; false to fall
    /// through to protocol SlashCommand::parse.
    ///
    /// The argless palette-select form delegates here from run_command; the
    /// arg-bearing forms typed in the input box are caught here first by
    /// submit_input (before SlashCommand::parse), so SlashCommand::parse
    /// never produces these variants from a typed command.
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
        // /search <query>: word-boundary match so /searchalot is NOT swallowed
        // as /search alot (it falls through to the model as a message). Follows
        // the /permissions word-boundary convention. Enters the full-screen verbose search
        // view (Scroll mode + verbose + the query). --all is recognized but
        // full-history search is not wired into this view yet -- say so rather
        // than silently degrading (the user asked for MORE, silent less is
        // worse than an error), then search the in-memory window.
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
        // /artifact-save [path]: persist the current (post-edit) document to
        // disk. Writes to the loaded path by default, or the given path. No
        // auto-save: explicit only. TUI-local (not in the protocol palette).
        if name == "artifact-save" || name.starts_with("artifact-save ") {
            if self.pane != Pane::Artifact {
                self.system_line("artifact-save: open an artifact first (/artifact <path>)");
                return true;
            }
            let arg = name.strip_prefix("artifact-save").unwrap_or("").trim();
            let path = if arg.is_empty() {
                self.artifact.path().to_string()
            } else {
                arg.to_string()
            };
            let lines = self.artifact.current_lines().len();
            match self.artifact.save(&path) {
                Ok(()) => self.system_line(format!("artifact: saved {path} ({lines} lines)")),
                Err(_) => self.system_line(format!("artifact: could not write {path}")),
            }
            return true;
        }
        // /artifact [path]: open the inline-review pane and load the document
        // from disk. TUI-local (not in the protocol palette). Falls back to the
        // canned stub when the path cannot be read. Always opens in Working
        // mode: artifact editing needs the input box, and Focus mode hides it
        // (which would dead-end the c/o/d keys).
        if name == "artifact" || name.starts_with("artifact ") {
            let arg = name.strip_prefix("artifact").unwrap_or("").trim();
            let path = if arg.is_empty() {
                DEFAULT_ARTIFACT_PATH
            } else {
                arg
            };
            match ArtifactSession::load(path) {
                Ok(session) => {
                    self.artifact = session;
                    self.pane = Pane::Artifact;
                    self.system_line(format!(
                        "artifact: opened {} (c=replace o=insert d=delete i=nl; a/r=review)",
                        self.artifact.path()
                    ));
                }
                Err(_) => {
                    self.pane = Pane::Artifact;
                    self.system_line(format!(
                        "artifact: could not read {path}, showing canned stub"
                    ));
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
