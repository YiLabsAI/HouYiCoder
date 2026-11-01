//! Applies the permission-setting replies the mode, rules, and git-ask reads
//! ask for. Each reply replaces the cached value the status bar and the panes
//! read; the settings whose change is worth a transcript line write it here,
//! when the host has answered, rather than when the request went out.

use houyicoder_protocol::frontend::permission::PermissionMode;
use houyicoder_protocol::frontend::permission::PermissionRule;

use crate::command::render::render_permission_rules_wire;
use crate::state::App;

impl App {
    /// Take the permission mode the host reports. Silent: the status bar
    /// reflects the new mode on the next render, and mode switching is a
    /// background state change rather than a conversation event.
    pub(crate) fn apply_permission_mode(&mut self, mode: PermissionMode) {
        self.sessions.current.capabilities.permission.mode_cache = Some(mode);
    }

    /// Replace the durable rule set and render it for the reader.
    pub(crate) fn apply_permission_rules(&mut self, rules: Vec<PermissionRule>) {
        self.sessions.current.capabilities.permission.rules_cache = rules.clone();
        self.system_line(render_permission_rules_wire(&rules));
    }

    /// Take the git-confirmation setting and say which way it now stands.
    pub(crate) fn apply_ask_before_git(&mut self, enabled: bool) {
        let permission = &mut self.sessions.current.capabilities.permission;
        permission.ask_before_git_enabled = enabled;
        self.system_line(format!(
            "permission: ask before git operations: {} (git commit/rebase/reset/tag {} before running)",
            if enabled { "on" } else { "off" },
            if enabled { "ask" } else { "run without asking" },
        ));
    }
}
