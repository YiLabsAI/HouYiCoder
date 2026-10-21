//! The working surface's input: routing a pasted token, submitting the input
//! box, and dispatching the commands that live entirely in the TUI.

use std::time::Instant;

use houyicoder_protocol::frontend::SlashCommand;

use crate::history::current_project;
use crate::input::queue::{InputItem, is_state_changing};
use crate::paste::PasteStore;
use crate::permission_input::submit_permission_input;
use crate::state::{App, Pane, Stage};

impl App {
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
            submit_permission_input(self, text);
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
        let text = PasteStore::expand(&text, &self.ui.input.pasted);
        if self.sessions.current.connection.is_some() {
            let project = current_project();
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
}
