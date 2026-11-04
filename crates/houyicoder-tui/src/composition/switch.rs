//! Session switch: commit a new session without rebuilding the whole App.
//! The session-local fields reset explicitly (reset_session_local), the new
//! session is committed via Sessions::set_current, and the process-level
//! resources (terminal focus, clipboard, layout feedback) stay. This
//! replaces the old rebuild-from-build_app safety net.

use super::*;

use crate::agent_message::ClientCommand;
use crate::input::queue::resume_command_target;
use std::env::var;
use std::mem::take;
use std::time::Instant;

use crate::ui::switch_state::{MAX_AUTO_WAKE_TURNS, WAKE_DEBOUNCE};

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
            self.ui.quit = true;
            false
        }
    }

    /// The event loop's idle drain. Drains a queued message when the prior
    /// run ended clean or a swap just landed; parks it on interrupt or error.
    /// Wakes the session for pending completion notifications on a debounce.
    pub fn idle_drain(&mut self, resume_builder: Option<&ResumeBuilderRef>, dirty: &mut bool) {
        if !self.agent_busy() && !self.shell_run_active() && !self.reverse_request_in_flight() {
            let switched = self.try_switch_session(resume_builder, dirty);
            if !self.ui.quit
                && (self.sessions.current.last_run_final || switched)
                && self.drain_pending_head()
            {
                *dirty = true;
            }
            if !self.ui.quit
                && (self.sessions.current.last_run_final || switched)
                && self.wake_notification_turn()
            {
                *dirty = true;
            }
        }
    }

    fn wake_notification_turn(&mut self) -> bool {
        let count = self.sessions.current.notifications_pending;
        if count == 0 {
            self.ui.notification_wake.pending_at_window = None;
            self.ui.notification_wake.window_started = None;
            return false;
        }
        let now = Instant::now();
        let window_open = {
            let wake = &self.ui.notification_wake;
            match (wake.pending_at_window, wake.window_started) {
                (Some(seen), Some(started)) if seen == count => {
                    now.duration_since(started) >= WAKE_DEBOUNCE
                }
                _ => {
                    self.ui.notification_wake.pending_at_window = Some(count);
                    self.ui.notification_wake.window_started = Some(now);
                    return false;
                }
            }
        };
        if !window_open {
            return false;
        }
        if self.ui.notification_wake.auto_wake_turns >= MAX_AUTO_WAKE_TURNS {
            return false;
        }
        let Some(s) = self.sessions.current.connection.as_ref() else {
            return false;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.ui.notification_wake.pending_at_window = None;
            self.ui.notification_wake.window_started = None;
            return false;
        };
        let session_id = self.sessions.current.identity.session_id.clone();
        let disabled_skills = self.sessions.current.capabilities.skills.disabled.clone();
        if let Err(e) = self.enqueue(ClientCommand::NotificationDrain {
            req_id,
            session_id,
            disabled_skills,
        }) {
            self.system_line(Self::enqueue_failure_line("wake", e));
            self.ui.notification_wake.pending_at_window = None;
            self.ui.notification_wake.window_started = None;
            return false;
        }
        let wake = &mut self.ui.notification_wake;
        wake.auto_wake_turns = wake.auto_wake_turns.saturating_add(1);
        wake.pending_at_window = None;
        wake.window_started = None;
        self.sessions.current.notifications_pending = 0;
        self.sessions.current.run.start(req_id, Instant::now());
        true
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
        // Taken before the reset below, which empties the owner this value
        // comes from: what the reader had open and where they stood are the
        // leaving session's, and they come back with it.
        let leaving_view = self.take_view_state();
        let mut parked = take(&mut self.ui.parked_view_states);
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
        // Read the pre-switch run outcome before set_current replaces the
        // session: a swap runs at idle after a clean run end or a
        // user-initiated picker pick, so the carried value is the true state
        // rather than a forged run result.
        let carried_last_run_final = self.sessions.current.last_run_final;

        // The new Session is fresh by construction, so reset only the
        // frontend-side state; process-level resources carry over.
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

        // Apply the session-scoped bundle data against the new session. The
        // sandbox is re-set to the wired-runner label (a swap always wires a
        // real runner); the applied model is committed beside it; and the run
        // outcome is carried from the pre-switch read above.
        self.sessions.current.sandbox =
            if var("HOUYICODER_SANDBOX_NO_ENFORCE").is_ok_and(|v| v == "1") {
                "off (no-sandbox)".to_string()
            } else {
                "mac-seatbelt".to_string()
            };
        self.sessions.current.last_run_final = carried_last_run_final;
        self.sessions.current.capabilities.model.catalog.applied.id = model;
        // A swap is always initiated from the working screen (the user is on
        // the transcript when they /resume), so force Working regardless of
        // the frontend startup policy. The landing screen and login mode
        // come from apply_frontend_startup, which the caller runs next.
        self.ui.navigation.screen = Screen::Working;
        self.ui.quit = false;
        self.ui.input.reset_composer_for_session_switch();

        self.sessions.current.todos.set_replaying_history(true);
        self.bump_transcript_version();
        parked.park_session(leaving, leaving_view);
        if let Some(state) = parked.take_session(&self.sessions.current.identity.session_id) {
            self.set_view_state(state);
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
        self.ui.input.apply_history_path(history_path.clone());
        self.ui.input.apply_shell_history_path(history_path);
    }

    /// Reset the frontend-side session-local fields a session switch must
    /// clear so the new session renders as if fresh. The fields the new
    /// Session owns (run, transcript, prompt, todos, capabilities,
    /// history_reads, search_reads, scan_state, agents, agent types) are
    /// fresh by construction in set_current, so this resets only what stays
    /// outside the new Session: the workflow stub and the Ui session-local state.
    /// The Ui owner holds its own reset rule (navigation overlays, pane
    /// states, caches, selection), and the process-level resources (terminal
    /// focus, clipboard) plus the render-measured frame feedback stay. The
    /// input and parked view states are carried by the caller at the field
    /// level because they mix session-scoped and carry-across fields.
    fn reset_session_local(&mut self) {
        self.workflow = app_default::workflow();
        self.ui.reset_for_session_switch();
    }
}
