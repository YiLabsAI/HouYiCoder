//! What the Config tab's Enter does to the row under the cursor.
//!
//! Each action reaches the same owner the corresponding command does, so a
//! value changed here and a value changed from a slash command cannot drift:
//! the permission mode asks the server to cycle it, the memory toggles ship
//! the toggle request, and the auto-approve timer changes the field the
//! approval card arms its countdown from.

use houyicoder_protocol::frontend::memory::MemoryToggleWhich;

use crate::state::{App, enums::StatusTab};
use crate::view::status::config_rows::{CONFIG_ROWS, ConfigRow, TIMER_STEPS, row_changes};

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
}
