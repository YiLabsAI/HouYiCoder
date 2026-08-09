//! /permissions + /goal command dispatch. Split out of command.rs on size
//! grounds. The permission surface (mode, durable rules, verdict log) is the
//! server's authority; these commands either echo a cached view or ship a
//! wire verb whose reply refreshes the cache.

use crate::state::App;
use houyicoder_protocol::frontend::permission::PermissionMode;

impl App {
    /// /permissions dispatch. Returns true when the name matched. Extracted
    /// from run_tui_local_command so that dispatcher stays under the
    /// too-many-lines gate.
    pub(crate) fn run_permission_command(&mut self, name: &str) -> bool {
        // /permissions add <action> <allow|deny|ask> | del <idx> | list | view:
        // manage the durable rule set that overrides the mode default, or view
        // the full permission surface (mode + rules + verdicts). Plural
        // matches the plural command name -- it manages many rules.
        if name == "permissions" || name.starts_with("permissions ") {
            let arg = name.strip_prefix("permissions").unwrap_or("").trim();
            let parts: Vec<&str> = arg.split_whitespace().collect();
            match parts.as_slice() {
                [] => self.open_permission_pane(),
                ["list"] => self.show_permission_rules(),
                ["view"] => self.show_permission_view(),
                ["add", action, effect] => self.add_permission_rule(action, effect),
                ["del", idx] => self.remove_permission_rule(idx),
                ["git"] => self.show_ask_before_git(),
                ["git", "on"] => self.request_ask_before_git(true),
                ["git", "off"] => self.request_ask_before_git(false),
                _ => self.system_line(
                    "permissions: usage /permissions add <action> <allow|deny|ask> | del <idx> | list | view | git [on|off]"
                        .to_string(),
                ),
            }
            return true;
        }
        false
    }

    /// /permissions list: show the durable rule set from the wire cache.
    fn show_permission_rules(&mut self) {
        let rules = &self.rules_cache;
        if rules.is_empty() {
            self.system_line("permission: no rules (mode defaults apply)");
            return;
        }
        let mut s = String::from("permission rules:");
        for (i, r) in rules.iter().enumerate() {
            s.push_str(&format!(
                "\n  [{i}] {} {}",
                r.action,
                super::render::permission_effect_label(r.effect)
            ));
        }
        self.system_line(s);
    }

    /// /permissions view: the full permission surface -- active mode, durable
    /// rules, and the session verdict log. Mode + rules come from the wire
    /// cache; the verdict log accumulates from the acpx
    /// permission_decision stream.
    fn show_permission_view(&mut self) {
        let mode = self.current_mode();
        let ask_before_git = self.ask_before_git_enabled;
        self.system_line(super::render::render_permission_view(
            mode,
            &self.rules_cache,
            &self.verdict_log_cache,
            ask_before_git,
        ));
    }

    /// /permissions (no arg) opens the interactive rule-manager pane. The pane
    /// is its own surface (tab header + rule list + footer nav hint), so it
    /// does not echo a system line into the transcript -- opening a view is a
    /// view switch, not a turn that needs an in-stream acknowledgment. Esc
    /// returns to the transcript.
    pub(crate) fn open_permission_pane(&mut self) {
        self.pane = crate::state::Pane::Permission;
    }

    /// /permissions add <action> <effect>: add a rule. The server is the
    /// authority; the wire PermissionAddRule verb carries the full rule and the
    /// server's reply refreshes the rules cache. Delegates to the shared
    /// parser so the pane's Add sub-mode and this command stay in sync.
    fn add_permission_rule(&mut self, action: &str, effect: &str) {
        crate::permission_input::add_rule_from_command(self, action, effect);
    }

    /// /permissions del <idx>: remove a rule by index. The server is the
    /// authority; the wire PermissionRemoveRule verb's reply refreshes the
    /// rules cache. Delegates to the shared removal path used by the pane's
    /// Remove sub-mode.
    fn remove_permission_rule(&mut self, idx: &str) {
        match idx.parse::<usize>() {
            Ok(i) => crate::permission_input::remove_rule_at(self, i),
            Err(_) => self.system_line("permission: usage /permissions del <idx>".to_string()),
        }
    }

    /// The current permission mode (from the wire cache the server fills).
    /// Defaults to Auto when no response has landed yet.
    pub fn current_mode(&self) -> PermissionMode {
        self.mode_cache.unwrap_or(PermissionMode::Auto)
    }

    /// Shift+Tab cycle of the permission mode: manual to auto and back. The
    /// destructive-command guardrail (rm / sudo / redirect) still fires in auto
    /// mode, so cycling into auto never silently allows a destructive call.
    pub fn tab_cycle_mode(&mut self) {
        // Shift+Tab cycles the mode; the status-bar pill reflects it on the
        // next render, so no system line is pushed -- the footer pill is the
        // single source of mode truth. The server is the authority; it cycles
        // + responds with the new mode, which lands in mode_cache. No-op in
        // stub mode (no carrier, no permission surface).
        if let Some(req_id) = self.mint_request_id() {
            self.send_cmd(crate::run_control::ClientCommand::PermissionCycleModeQuery { req_id });
        }
    }

    /// /permissions git: show whether git commit/rebase/reset/tag ask before
    /// running (from the cache; a server round-trip refreshes it via
    /// PermissionAskBeforeGitResult).
    fn show_ask_before_git(&mut self) {
        let on = self.ask_before_git_enabled;
        self.system_line(format!(
            "permission: ask before git operations: {} (git commit/rebase/reset/tag {} before running; /permissions git on|off to toggle)",
            if on { "on" } else { "off" },
            if on { "ask" } else { "run without asking" },
        ));
        // Refresh from the server so the cache reflects the gate's authority.
        if let Some(req_id) = self.mint_request_id() {
            self.send_cmd(
                crate::run_control::ClientCommand::PermissionAskBeforeGitQuery {
                    req_id,
                    enabled: None,
                },
            );
        }
    }

    /// /permissions git on|off: toggle whether git commit/rebase/reset/tag ask
    /// before running. The server is the authority; the reply
    /// (PermissionAskBeforeGitResult) refreshes the cache + surfaces the state.
    fn request_ask_before_git(&mut self, enabled: bool) {
        if let Some(req_id) = self.mint_request_id() {
            self.send_cmd(
                crate::run_control::ClientCommand::PermissionAskBeforeGitQuery {
                    req_id,
                    enabled: Some(enabled),
                },
            );
        } else {
            // No server: update the cache optimistically (stub mode).
            self.ask_before_git_enabled = enabled;
            self.system_line(format!(
                "permission: ask before git operations: {}",
                if enabled { "on" } else { "off" }
            ));
        }
    }
}
