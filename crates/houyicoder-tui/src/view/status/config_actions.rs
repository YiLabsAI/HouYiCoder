//! What the Config tab's Enter does to the row under the cursor.
//!
//! Each action reaches the same owner the corresponding command does, so a
//! value changed here and a value changed from a slash command cannot drift:
//! the permission mode asks the server to cycle it, the memory toggles ship
//! the toggle request, and the auto-approve timer changes the field the
//! approval card arms its countdown from.

use houyicoder_protocol::frontend::memory::MemoryToggleWhich;

use crate::agent_message::ClientCommand;

use crate::state::{App, enums::StatusTab};
use crate::view::status::config_rows::{
    CONFIG_ROWS, ConfigRow, RECAP_IDLE_STEPS, TIMER_STEPS, row_changes,
};

impl App {
    /// Place the Config cursor when the tab becomes active, and drop it when
    /// the tab leaves. A cursor kept while another tab is showing would let a
    /// later Up/Down move a list nobody can see.
    pub(crate) fn sync_config_cursor(&mut self) {
        self.ui.navigation.status_pane.config_cursor =
            (self.ui.navigation.status_pane.tab == StatusTab::Config).then_some(0);
    }
    /// Move the Config tab's cursor by one row, wrapping at both ends. The
    /// list is short and its order is fixed, so wrapping costs the reader
    /// nothing and saves a dead press at the bottom.
    pub(crate) fn move_config_cursor(&mut self, delta: i32) {
        let len = CONFIG_ROWS.len();
        let current = self.ui.navigation.status_pane.config_cursor.unwrap_or(0);
        let next = (current as i32 + delta).rem_euclid(len as i32) as usize;
        self.ui.navigation.status_pane.config_cursor = Some(next);
    }

    /// The row the Config cursor sits on, once the tab has one.
    pub(crate) fn config_cursor_row(&self) -> Option<ConfigRow> {
        self.ui
            .navigation
            .status_pane
            .config_cursor
            .and_then(|i| CONFIG_ROWS.get(i).copied())
    }

    /// Act on the row under the Config cursor. A row that shows state the
    /// session does not own takes no press and says nothing.
    pub(crate) fn activate_config_row(&mut self) {
        let Some(row) = self.config_cursor_row() else {
            return;
        };
        if !row_changes(row) {
            return;
        }
        match row {
            ConfigRow::PermissionMode => self.tab_cycle_mode(),
            ConfigRow::AutoApproveTimer => self.step_auto_approve_timer(),
            ConfigRow::AutoMemory => self.toggle_memory_setting(MemoryToggleWhich::Auto),
            ConfigRow::AutoDream => self.toggle_memory_setting(MemoryToggleWhich::Dream),
            ConfigRow::Recap => self.set_recap_config(),
            ConfigRow::RecapIdle => self.step_recap_idle(),
            // Model and Sandbox are shown, not changed: the model picker is
            // its own pane, and the sandbox is fixed for the session.
            ConfigRow::Model | ConfigRow::ReasoningEffort | ConfigRow::Sandbox => {}
        }
    }

    /// Advance the auto-approve countdown to the next step in the ring.
    ///
    /// The card arms its countdown from this field, so writing it is the whole
    /// effect: the next approval in this session waits the new number of
    /// seconds. A value that is not one of the steps (an environment override
    /// naming an odd delay) moves to the first step rather than searching for
    /// a neighbour it does not have.
    fn step_auto_approve_timer(&mut self) {
        let capability = &mut self.sessions.current.capabilities.permission;
        let current = capability.auto_approve_delay_secs;
        let next = TIMER_STEPS
            .iter()
            .position(|step| *step == current)
            .map(|i| TIMER_STEPS[(i + 1) % TIMER_STEPS.len()])
            .unwrap_or(TIMER_STEPS[0]);
        capability.auto_approve_delay_secs = next;
        let shown = if next == 0 {
            "off".to_string()
        } else {
            format!("{next}s")
        };
        self.system_line(format!("auto-approve timer: {shown}"));
    }

    /// Flip the recap switch, sending the window along with it.
    ///
    /// The host owns the settings file, so the tab asks rather than writing:
    /// a local write would be overwritten by the next host save and would
    /// diverge from the file the recap actually reads.
    fn set_recap_config(&mut self) {
        let enabled = !self.snapshot_or_stub().recap_enabled;
        let idle = self.snapshot_or_stub().recap_idle_secs;
        self.send_recap_config(enabled, idle);
    }

    /// Advance the idle window to the next step in the ring, keeping the
    /// switch as it is. A window outside the ring moves to the first step,
    /// the same way the timer row handles an unexpected value.
    fn step_recap_idle(&mut self) {
        let snap = self.snapshot_or_stub();
        let enabled = snap.recap_enabled;
        let current = snap.recap_idle_secs;
        let next = RECAP_IDLE_STEPS
            .iter()
            .position(|step| *step == current)
            .map(|i| RECAP_IDLE_STEPS[(i + 1) % RECAP_IDLE_STEPS.len()])
            .unwrap_or(RECAP_IDLE_STEPS[0]);
        self.send_recap_config(enabled, next);
    }

    /// Ask the host to write both recap fields and echo the outcome.
    fn send_recap_config(&mut self, enabled: bool, idle_secs: u64) {
        let Some(s) = self.sessions.current.connection.as_ref() else {
            self.system_line("recap: not connected");
            return;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("recap: request ids exhausted");
            return;
        };
        if let Err(e) = self.enqueue(ClientCommand::RecapConfigSet {
            req_id,
            enabled,
            idle_secs,
        }) {
            self.system_line(App::enqueue_failure_line("recap", e));
            return;
        }
        let state = if enabled { "on" } else { "off" };
        self.system_line(format!("recap: {state} · idle after {}m", idle_secs / 60));
    }
}
