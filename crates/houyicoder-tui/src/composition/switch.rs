//! Session switch: commit a new session without rebuilding the whole App.
//! The session-local fields reset explicitly (reset_session_local), the new
//! session is committed via Sessions::set_current, and the process-level
//! resources (terminal focus, clipboard, layout feedback) stay. This
//! replaces the old rebuild-from-build_app safety net.

use super::*;

use crate::history::HistoryNav;
use crate::input::queue::resume_command_target;

impl App {
    /// Try to consume a queued /resume Command at the head and swap the
    /// session. Returns true when a swap landed, so the caller's queue drain
    /// can treat the swap itself as a clean transition without forging
    /// last_run_final: a carried pending item drains in the new session
    /// because the swap happened at idle after a clean run end or a
    /// user-initiated pick, not because a forged run result says so. The
    /// /resume Command at the head is the cross-tick carrier (run_resume
    /// enqueues it with the resolved target).
    pub fn try_switch_session(
        &mut self,
        resume_builder: Option<&ResumeBuilderRef>,
        dirty: &mut bool,
    ) -> bool {
        let Some(target) = self
            .ui
            .input
            .queue
            .first()
            .and_then(resume_command_target)
            .map(str::to_string)
        else {
            return false;
        };
        // A resume target that names the session already presented is a
        // no-op: the live lease already holds that sid's lock, so
        // re-acquiring it would report the lock held by this process itself.
        let current = self.sessions.current.identity.session_id.0.clone();
        if target == current {
            self.ui.input.queue.remove(0);
            self.system_line(format!("already on session {target}"));
            *dirty = true;
            return false;
        }
        // The caller (the event loop's idle guard) already gates on
        // !agent_busy && !reverse_request_in_flight, so a target that
        // survives to here is ready to swap.
        if let Some(builder) = resume_builder {
            match builder(&target) {
                Ok((new_bundle, startup)) => {
                    self.ui.input.queue.remove(0);
                    self.switch_session(new_bundle);
                    self.apply_frontend_startup(startup);
                    *dirty = true;
                    true
                }
                Err(e) => {
                    self.ui.input.queue.remove(0);
                    self.system_line(format!("resume failed: {e}"));
                    *dirty = true;
                    false
                }
            }
        } else {
            // No in-process builder: leave the /resume Command queued and
            // quit so the caller reads the target from the queue and
            // rebuilds the session outside this loop.
            self.quit = true;
            false
        }
    }

    /// The event loop's idle drain: continuous-state polling + consumptive
    /// idempotency. !agent_busy holds every frame; no flood because drain/take
    /// CONSUMES the item, so the next frame has nothing to do. A queued item
    /// auto-sends when the prior run ended FinalOutput (a clean end) OR when a
    /// session swap just landed in this idle step -- the swap itself is a
    /// clean transition (it runs at idle after a clean end or a user pick), so
    /// a carried pending item drains in the new session without forging
    /// last_run_final. An interrupt/error does NOT auto-send: the queued item
    /// stays parked for the user to recall to the input box, edit, and re-send.
    pub fn idle_drain(&mut self, resume_builder: Option<&ResumeBuilderRef>, dirty: &mut bool) {
        if !self.agent_busy() && !self.reverse_request_in_flight() {
            let switched = self.try_switch_session(resume_builder, dirty);
            // A no-builder try_switch_session sets quit and leaves the /resume
            // queued for the external caller; draining it here would remove +
            // re-dispatch the command, so the guard keeps the target on the
            // queue for the loop-exit drain.
            if !self.quit && (self.status.last_run_final || switched) && self.drain_pending_head() {
                *dirty = true;
            }
        }
    }

    pub fn switch_session(&mut self, bundle: RunnerBundle) {
        // A swap commits a new session without rebuilding the whole App.
        // reset_session_local clears the session-local fields, set_current
        // commits the new session, and process-level resources (focus,
        // clipboard, layout feedback) stay. Bump transcript_version
        // so a stale cached render does not ride into the new session.
        if self.ui.teammate_view.is_some() {
            self.exit_teammate_view();
        }
        // The pending queue is carried across (only session-scoped fields
        // reset); the queue owns the carry rules and reports the dropped
        // command texts for the notice at the end of the swap.
        let dropped = self.ui.input.queue.transfer_across_sessions();
        let leaving = self.sessions.current.identity.session_id.clone();
        let open_keys = self.take_expanded_keys();
        let mut parked = std::mem::take(&mut self.ui.parked_view_states);
        // Deconstruct the bundle: the session-construction half (client,
        // channels, id) builds the new Session; the host resources
        // (trajectory, export, snapshot, catalog) are applied to Sessions.
        // The frontend startup policy (skip-login, warnings, history path)
        // is not here — the caller applies it via apply_frontend_startup so
        // the session commit does not mix lifecycle domains.
        let RunnerBundle {
            client,
            agent_tx,
            agent_rx,
            session: session_id,
            model,
            trajectory_log,
            export_log,
            snapshot,
            session_catalog,
        } = bundle;
        let driver_agent_tx = agent_tx.clone();
        let live_session =
            SessionConnection::spawn(client, driver_agent_tx, agent_rx, &shared_runtime());
        let mut new_session = Session::bare(SessionIdentity {
            session_id,
            working_dir: cwd_string(),
        });
        new_session.connection = Some(live_session);

        // Reset session-local UI caches and pane state explicitly; the new
        // Session is fresh by construction so only App-side caches need it.
        // terminal_focused and clipboard are process resources (not reset);
        // layout_feedback is render-measured (rewritten each draw). verbose
        // resets below: it is a search-view lifecycle flag, not a process
        // resource.
        self.reset_session_local();

        // Commit the new session in one step: no draw or message processing
        // between the reset and the commit.
        self.sessions.set_current(new_session);
        self.sessions.persistence = SessionPersistence {
            trajectory_log,
            export_log,
            snapshot,
        };
        self.sessions.catalog = session_catalog;

        // Apply the non-session bundle data against App fields. status is
        // mixed: model and sandbox are session-scoped (re-set from the
        // bundle), tokens and plan_mode reset to their factory defaults, and
        // last_run_final is carried from the pre-switch value. A swap runs at
        // idle after a clean run end or a user-initiated picker pick, so the
        // carried value is the true state, not a forged run result -- the old
        // re-set to true is gone, and the idle drain's switched flag carries
        // the queued item instead.
        let carried_last_run_final = self.status.last_run_final;
        self.status = default_status();
        self.status.model = model.clone();
        self.status.sandbox = "mac-seatbelt".to_string();
        self.status.last_run_final = carried_last_run_final;
        self.sessions.current.capabilities.model.catalog.applied.id = model;
        // A swap is always initiated from the working screen (the user is on
        // the transcript when they /resume), so force Working regardless of
        // the frontend startup policy. The landing screen and login mode
        // come from apply_frontend_startup, which the caller runs next.
        self.ui.navigation.screen = Screen::Working;
        self.quit = false;
        self.ui.input.editor = Default::default();
        self.ui.input.pasted = Default::default();
        self.ui.input.pending_paste = Default::default();

        self.sessions.current.todos.set_replaying_history(true);
        self.bump_transcript_version();
        parked.park_session(leaving, open_keys);
        if let Some(keys) = parked.take_session(&self.sessions.current.identity.session_id) {
            self.set_expanded_keys(keys);
        }
        // The new session's fleet is unrelated to the old one's children,
        // and a child id can repeat across sessions: no parked child
        // view state crosses the switch.
        parked.clear_children();
        self.ui.parked_view_states = parked;
        if !dropped.is_empty() {
            self.system_line(format!(
                "resume: dropped {} command(s) from the old session ({}) -- re-issue in the new session if needed",
                dropped.len(),
                dropped.join(", ")
            ));
        }
    }

    /// Apply the frontend startup policy the composition root resolved:
    /// skip-login sets the landing screen and login mode, startup warnings
    /// land as initial transcript system lines before any run output, and
    /// the history path wires the prompt-history file. Separate from the
    /// session commit so a Session never carries frontend policy.
    pub fn apply_frontend_startup(&mut self, startup: FrontendStartup) {
        let FrontendStartup {
            skip_login,
            startup_warnings,
            history_path,
        } = startup;
        if skip_login {
            self.ui.navigation.screen = Screen::Working;
            self.ui.navigation.login_mode = Some(LoginMode::Local);
        }
        // Push the drained startup warnings as initial system lines BEFORE any
        // run output — synchronous so they land first + do not race with
        // command results (the async-sink path flaked timing tests; this is
        // deterministic).
        for w in startup_warnings {
            self.system_line(w);
        }
        self.ui.input.history = HistoryNav::with_path(history_path);
    }

    /// Reset the App-side session-local fields a session switch must clear so
    /// the new session renders as if fresh rather than layered over the old
    /// one's pane state, caches, or selection. The fields the new Session owns
    /// (run, transcript, prompt, todos, capabilities, history_reads,
    /// index_build, agents, agent_directory) are fresh by construction in
    /// set_current, so this resets only what stays on App: the transcript view
    /// state, the render and row caches, the pane drill-downs, the selection,
    /// the fleet strip, and the verbose flag. The process-level resources
    /// (terminal_focused, clipboard) and the render-measured feedback
    /// (layout_feedback, rewritten each draw) are intentionally absent: a
    /// switch does not impersonate a cleanup of terminal focus or the
    /// clipboard, and layout_feedback self-corrects. status and input are
    /// handled by the caller at the field level because they mix
    /// session-scoped, run-state, and carry-across fields.
    fn reset_session_local(&mut self) {
        self.workflow = app_default::workflow();
        self.ui.navigation.pane = Pane::Transcript;
        self.ui.navigation.viewport = ViewportMode::Working;
        self.ui.navigation.prev_viewport = ViewportMode::Working;
        self.ui.transcript_view = Default::default();
        self.ui.verbose = false;
        self.display_rows_cache = Default::default();
        self.display_rows_cache.version.set(u64::MAX);
        self.ui.navigation.resume_picker = Default::default();
        self.ui.navigation.palette = Default::default();
        self.ui.queue_view = Default::default();
        self.ui.worktree_list = Default::default();
        self.ui.worktree_level = Cell::new(0);
        self.ui.trajectory = Default::default();
        self.ui.agents_sel = 0;
        self.ui.notifications = Default::default();
        self.ui.displayed_tokens = Cell::new(0);
        self.ui.selection = Default::default();
        self.layout_snapshot = Default::default();
        self.render_cache = RefCell::new(Default::default());
        self.last_row_keys = Default::default();
        self.ui.teammate_view = None;
        self.ui.fleet = Default::default();
        self.ui.permission_pane = Default::default();
        self.ui.skills_pane = Default::default();
        self.ui.navigation.skill_picker = Default::default();
        self.ui.hooks_pane = Default::default();
        self.ui.memory = Default::default();
        self.ui.model = Default::default();
        self.ui.navigation.status_tab = Default::default();
        self.ui.navigation.status_name_edit = None;
        self.ui.last_title = None;
    }
}
