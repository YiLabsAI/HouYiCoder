//! The /permissions pane input sub-modes: add a rule (spec text then a
//! destination pick), confirm a removal (Yes/No), and live search-filter the
//! current tab. None = list navigation. Search owns a pane-local buffer so it
//! never eats a slash command's leading slash; AddDestination and Remove are
//! Select-style (Left/Right + Enter) and own their keys. Add (spec text) is
//! the only sub-mode that falls through to the main input box for typing.

use std::ptr::eq;

use houyicoder_protocol::frontend::permission::{
    PermissionEffect, PermissionRule, PermissionRuleContent, RuleDestination,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::agent_message::ClientCommand;
use crate::command::render::permission_effect_label;
use crate::permission_pane_state::PermissionPaneState;
use crate::state::{App, Pane, PermissionInput, PermissionTab};

/// A parsed rule spec: action + optional content tier + effect. The wire
/// rule is built from this so the Add sub-mode and the /permission add
/// command share one parse path.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PermissionRuleSpec {
    pub action: String,
    pub content: Option<PermissionRuleContent>,
    pub effect: PermissionEffect,
}

/// What the permission pane decided the shell should do with a key.
///
/// The pane owns its tab, cursor, sub-mode, and search buffer, so its own
/// transitions run on the pane state alone and return this value for the shell
/// to ship any command or clear the shared editor.
pub(crate) enum PermissionDecision {
    /// The key falls through to the main input handler (Add or AddDir text).
    PassThrough,
    /// The pane handled the key; clear_editor clears the main input box when
    /// entering or leaving a typed sub-mode.
    Consumed { clear_editor: bool },
    /// Leave the permission pane back to the transcript.
    ClosePane,
    /// Ship a new rule to the server with the chosen destination.
    AddRule {
        spec: PermissionRuleSpec,
        destination: RuleDestination,
    },
    /// Remove the rule at this index in the unfiltered rules cache.
    RemoveRule(usize),
    /// Remove the working directory at this index in the directories cache.
    RemoveDir(usize),
    /// Emit a cancellation notice line into the transcript.
    CancelNotice(&'static str),
}

/// Parse a typed rule spec. Two shapes: action + content:effect (content
/// present) or action + effect (tool-level). Content tier by delimiter: a
/// quoted token is an Exact match, anything else is a Prefix.
pub(crate) fn parse_rule_spec(s: &str) -> Result<PermissionRuleSpec, String> {
    let mut iter = s.split_whitespace();
    let action = iter
        .next()
        .ok_or_else(|| "empty tool name".to_string())?
        .to_string();
    if action.is_empty() {
        return Err("empty tool name".to_string());
    }
    let second = match iter.next() {
        Some(t) => t,
        None => return Err("missing effect (allow, ask, or deny)".to_string()),
    };
    let (content, effect_str) = match second.rsplit_once(':') {
        Some((c, e)) if !c.is_empty() && !e.is_empty() => {
            let tier = if c.starts_with('"') && c.ends_with('"') && c.len() >= 2 {
                PermissionRuleContent::Exact {
                    value: c[1..c.len() - 1].to_string(),
                }
            } else {
                PermissionRuleContent::Prefix {
                    value: c.to_string(),
                }
            };
            (Some(tier), e)
        }
        _ => (None, second),
    };
    let effect = match effect_str.to_ascii_lowercase().as_str() {
        "allow" | "yes" => PermissionEffect::Allow,
        "deny" | "reject" | "no" => PermissionEffect::Reject,
        "ask" => PermissionEffect::Ask,
        other => {
            return Err(format!("unknown effect {other} (allow, ask, or deny)"));
        }
    };
    Ok(PermissionRuleSpec {
        action,
        content,
        effect,
    })
}

/// The rules shown in the current tab, filtered by the live search query when
/// the Search sub-mode is active. Returns references into rules_cache.
pub(crate) fn filtered_rules<'a>(
    tab: PermissionTab,
    rules: &'a [PermissionRule],
    search: &str,
) -> Vec<&'a PermissionRule> {
    let want = match tab {
        PermissionTab::Allow => PermissionEffect::Allow,
        PermissionTab::Ask => PermissionEffect::Ask,
        PermissionTab::Deny => PermissionEffect::Reject,
        // Recent shows the denial log, not rules; Workspace lists directories.
        // Both return no rule rows — their bodies render from other caches.
        PermissionTab::Recent | PermissionTab::Workspace => return Vec::new(),
    };
    let needle = search.trim_ascii().to_ascii_lowercase();
    rules
        .iter()
        .filter(|r| r.effect == want)
        .filter(|r| {
            needle.is_empty()
                || r.action.to_ascii_lowercase().contains(&needle)
                || content_text(r).to_ascii_lowercase().contains(&needle)
        })
        .collect()
}

/// The live search query: the pane-local permission_pane.search buffer while
/// Search is active, an empty string otherwise (so Add/Remove/nav never
/// filter the list).
pub(crate) fn search_query(app: &App) -> &str {
    if matches!(app.ui.permission_pane.input, PermissionInput::Search) {
        &app.ui.permission_pane.search
    } else {
        ""
    }
}

/// The index into rules_cache of the cursor-th filtered rule, for removal.
pub(crate) fn filtered_rule_index(app: &App) -> Option<usize> {
    if matches!(
        app.ui.permission_pane.tab,
        PermissionTab::Recent | PermissionTab::Workspace
    ) {
        return None;
    }
    let view = filtered_rules(
        app.ui.permission_pane.tab,
        &app.sessions.current.capabilities.permission.rules_cache,
        search_query(app),
    );
    let cursor = app
        .ui
        .permission_pane
        .cursor
        .min(view.len().saturating_sub(1));
    let target = view.get(cursor).copied()? as *const PermissionRule;
    app.sessions
        .current
        .capabilities
        .permission
        .rules_cache
        .iter()
        .position(|r| eq(r, target))
}

/// The display text for a rule's content tier (matches the render path).
fn content_text(r: &PermissionRule) -> String {
    match &r.content {
        Some(PermissionRuleContent::Exact { value }) => format!("\"{value}\""),
        Some(PermissionRuleContent::Prefix { value }) => format!("{value}*"),
        Some(PermissionRuleContent::Glob { value }) => value.clone(),
        None => "(any)".to_string(),
    }
}

/// Handle a key in the /permission pane. Returns true when consumed (nav,
/// entry keys, or the sub-mode Esc exit); false lets the key fall through to
/// the generic input handler so Add/Remove typing + Enter still work.
pub(crate) fn handle_permission_key(app: &mut App, k: KeyEvent) -> bool {
    if app.ui.navigation.pane != Pane::Permission {
        return false;
    }
    let editor_empty = app.ui.input.editor.is_empty();
    let rule_idx = filtered_rule_index(app);
    let dirs_len = app
        .sessions
        .current
        .capabilities
        .permission
        .dirs_cache
        .len();
    let decision = app
        .ui
        .permission_pane
        .handle_key(k.code, editor_empty, rule_idx, dirs_len);
    match decision {
        PermissionDecision::PassThrough => false,
        PermissionDecision::Consumed { clear_editor } => {
            if clear_editor {
                app.ui.input.editor.clear();
            }
            true
        }
        PermissionDecision::ClosePane => {
            app.ui.navigation.pane = Pane::Transcript;
            true
        }
        PermissionDecision::AddRule { spec, destination } => {
            app.ui.input.editor.clear();
            app.add_rule_with_destination(spec, destination);
            true
        }
        PermissionDecision::RemoveRule(idx) => {
            app.ui.input.editor.clear();
            app.remove_rule_at(idx);
            true
        }
        PermissionDecision::RemoveDir(idx) => {
            app.ui.input.editor.clear();
            app.remove_dir_at(idx);
            true
        }
        PermissionDecision::CancelNotice(msg) => {
            app.ui.input.editor.clear();
            app.system_line(msg);
            true
        }
    }
}

impl PermissionPaneState {
    /// Clear the active sub-mode and search buffer.
    pub(crate) fn clear_input(&mut self) {
        self.input = PermissionInput::None;
        self.search.clear();
    }

    /// Apply one key to the permission pane state and report the shell action.
    pub(crate) fn handle_key(
        &mut self,
        code: KeyCode,
        editor_empty: bool,
        rule_idx: Option<usize>,
        dirs_len: usize,
    ) -> PermissionDecision {
        if code == KeyCode::Esc {
            if self.input.is_active() {
                self.clear_input();
                return PermissionDecision::Consumed { clear_editor: true };
            }
            return PermissionDecision::ClosePane;
        }
        match self.input {
            PermissionInput::Search => self.handle_search_keys(code),
            PermissionInput::AddDestination { .. }
            | PermissionInput::Remove { .. }
            | PermissionInput::RemoveDir { .. } => self.handle_select_keys(code),
            PermissionInput::Add | PermissionInput::AddDir => PermissionDecision::PassThrough,
            PermissionInput::None => self.handle_nav_keys(code, editor_empty, rule_idx, dirs_len),
        }
    }

    /// Select-style keys for the AddDestination and Remove-confirm steps:
    /// Left/Right cycle the focused option, Enter commits, other keys are
    /// swallowed so the main input box is never touched.
    fn handle_select_keys(&mut self, code: KeyCode) -> PermissionDecision {
        match &mut self.input {
            PermissionInput::AddDestination {
                action,
                content,
                effect,
                destination,
            } => match code {
                KeyCode::Left => {
                    *destination = prev_destination(*destination);
                    PermissionDecision::Consumed {
                        clear_editor: false,
                    }
                }
                KeyCode::Right => {
                    *destination = next_destination(*destination);
                    PermissionDecision::Consumed {
                        clear_editor: false,
                    }
                }
                KeyCode::Enter => {
                    let spec = PermissionRuleSpec {
                        action: action.clone(),
                        content: content.clone(),
                        effect: *effect,
                    };
                    let destination = *destination;
                    self.clear_input();
                    PermissionDecision::AddRule { spec, destination }
                }
                _ => PermissionDecision::Consumed {
                    clear_editor: false,
                },
            },
            PermissionInput::Remove { idx, confirm } => match code {
                KeyCode::Left | KeyCode::Right => {
                    *confirm = !*confirm;
                    PermissionDecision::Consumed {
                        clear_editor: false,
                    }
                }
                KeyCode::Enter => {
                    let (idx, confirm) = (*idx, *confirm);
                    self.clear_input();
                    if confirm {
                        PermissionDecision::RemoveRule(idx)
                    } else {
                        PermissionDecision::CancelNotice("permission: removal cancelled")
                    }
                }
                _ => PermissionDecision::Consumed {
                    clear_editor: false,
                },
            },
            PermissionInput::RemoveDir { idx, confirm } => match code {
                KeyCode::Left | KeyCode::Right => {
                    *confirm = !*confirm;
                    PermissionDecision::Consumed {
                        clear_editor: false,
                    }
                }
                KeyCode::Enter => {
                    let (idx, confirm) = (*idx, *confirm);
                    self.clear_input();
                    if confirm {
                        PermissionDecision::RemoveDir(idx)
                    } else {
                        PermissionDecision::CancelNotice("permission: directory removal cancelled")
                    }
                }
                _ => PermissionDecision::Consumed {
                    clear_editor: false,
                },
            },
            _ => PermissionDecision::PassThrough,
        }
    }

    /// Search sub-mode keys: printable chars + Backspace edit the pane-local
    /// search buffer (live filter); Enter ends search (filter clears);
    /// Up/Down navigate the filtered list. Everything else is swallowed.
    fn handle_search_keys(&mut self, code: KeyCode) -> PermissionDecision {
        match code {
            KeyCode::Char(c) => {
                self.search.push(c);
                self.cursor = 0;
                PermissionDecision::Consumed {
                    clear_editor: false,
                }
            }
            KeyCode::Backspace => {
                self.search.pop();
                self.cursor = 0;
                PermissionDecision::Consumed {
                    clear_editor: false,
                }
            }
            KeyCode::Enter => {
                self.clear_input();
                PermissionDecision::Consumed { clear_editor: true }
            }
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                PermissionDecision::Consumed {
                    clear_editor: false,
                }
            }
            KeyCode::Down => {
                self.cursor = self.cursor.saturating_add(1);
                PermissionDecision::Consumed {
                    clear_editor: false,
                }
            }
            _ => PermissionDecision::Consumed {
                clear_editor: false,
            },
        }
    }

    /// List navigation + sub-mode entry keys (no sub-mode active).
    fn handle_nav_keys(
        &mut self,
        code: KeyCode,
        editor_empty: bool,
        rule_idx: Option<usize>,
        dirs_len: usize,
    ) -> PermissionDecision {
        let on_rule_tab = matches!(
            self.tab,
            PermissionTab::Allow | PermissionTab::Ask | PermissionTab::Deny
        );
        match code {
            KeyCode::Left => {
                self.prev_tab();
                PermissionDecision::Consumed {
                    clear_editor: false,
                }
            }
            KeyCode::Right | KeyCode::Tab => {
                self.next_tab();
                PermissionDecision::Consumed {
                    clear_editor: false,
                }
            }
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                PermissionDecision::Consumed {
                    clear_editor: false,
                }
            }
            KeyCode::Down => {
                self.cursor = self.cursor.saturating_add(1);
                PermissionDecision::Consumed {
                    clear_editor: false,
                }
            }
            KeyCode::Char('a') if editor_empty && on_rule_tab => {
                self.input = PermissionInput::Add;
                PermissionDecision::Consumed { clear_editor: true }
            }
            KeyCode::Char('a') if editor_empty && self.tab == PermissionTab::Workspace => {
                self.input = PermissionInput::AddDir;
                PermissionDecision::Consumed { clear_editor: true }
            }
            KeyCode::Char('d') if editor_empty && on_rule_tab => {
                if let Some(idx) = rule_idx {
                    self.input = PermissionInput::Remove {
                        idx,
                        confirm: false,
                    };
                    PermissionDecision::Consumed { clear_editor: true }
                } else {
                    PermissionDecision::Consumed {
                        clear_editor: false,
                    }
                }
            }
            KeyCode::Char('d') if editor_empty && self.tab == PermissionTab::Workspace => {
                if dirs_len > 0 {
                    let idx = self.cursor.min(dirs_len.saturating_sub(1));
                    self.input = PermissionInput::RemoveDir {
                        idx,
                        confirm: false,
                    };
                    PermissionDecision::Consumed { clear_editor: true }
                } else {
                    PermissionDecision::Consumed {
                        clear_editor: false,
                    }
                }
            }
            KeyCode::Char('s') if editor_empty && on_rule_tab => {
                self.input = PermissionInput::Search;
                self.search.clear();
                self.cursor = 0;
                PermissionDecision::Consumed {
                    clear_editor: false,
                }
            }
            _ => PermissionDecision::PassThrough,
        }
    }
}

impl App {
    /// /permission add <action> <effect>: parse the two args through the shared
    /// spec parser (so bash npm:allow works from the command too) and ship.
    pub(crate) fn add_rule_from_command(&mut self, action: &str, effect: &str) {
        let spec = match parse_rule_spec(&format!("{action} {effect}")) {
            Ok(s) => s,
            Err(msg) => {
                self.system_line(format!("permission: {msg}"));
                return;
            }
        };
        self.add_rule_with_destination(spec, RuleDestination::default());
    }

    /// Ship a parsed rule to the server with the chosen destination
    /// (persistence scope).
    pub(crate) fn add_rule_with_destination(
        &mut self,
        spec: PermissionRuleSpec,
        destination: RuleDestination,
    ) {
        let wire_rule = PermissionRule {
            action: spec.action,
            content: spec.content,
            effect: spec.effect,
            destination,
        };
        let label = permission_effect_label(spec.effect);
        let action = wire_rule.action.clone();
        let Some(s) = self.sessions.current.connection.as_ref() else {
            self.system_line("permission: not connected");
            return;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("permission: request ids exhausted");
            return;
        };
        if let Err(e) = self.enqueue(ClientCommand::PermissionAddRuleQuery {
            req_id,
            rule: wire_rule,
        }) {
            self.system_line(Self::enqueue_failure_line("permission", e));
            return;
        }
        self.system_line(format!("permission: added {action} {label}"));
    }

    /// Remove the rule at the given index into the full rules_cache (the server
    /// sees the unfiltered set, not the tab-filtered view).
    pub(crate) fn remove_rule_at(&mut self, index: usize) {
        let Some(s) = self.sessions.current.connection.as_ref() else {
            self.system_line("permission: not connected");
            return;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("permission: request ids exhausted");
            return;
        };
        if let Err(e) = self.enqueue(ClientCommand::PermissionRemoveRuleQuery { req_id, index }) {
            self.system_line(Self::enqueue_failure_line("permission", e));
            return;
        }
        self.system_line("permission: removed".to_string());
    }

    /// Submit the Add / AddDir sub-mode's typed text (called from submit_input
    /// when Enter falls through).
    pub(crate) fn submit_permission_input(&mut self, text: String) {
        match self.ui.permission_pane.input.clone() {
            PermissionInput::Add => match parse_rule_spec(&text) {
                Ok(spec) => {
                    self.ui.input.editor.clear();
                    self.ui.permission_pane.input = PermissionInput::AddDestination {
                        action: spec.action,
                        content: spec.content,
                        effect: spec.effect,
                        destination: RuleDestination::default(),
                    };
                }
                Err(msg) => {
                    self.system_line(format!("permission: {msg}"));
                    self.ui.permission_pane.clear_input();
                    self.ui.input.editor.clear();
                }
            },
            PermissionInput::AddDir => {
                let path = text.trim().to_string();
                self.ui.permission_pane.clear_input();
                self.ui.input.editor.clear();
                if path.is_empty() {
                    self.system_line("permission: empty directory path");
                } else {
                    self.submit_add_dir(path);
                }
            }
            PermissionInput::AddDestination { .. }
            | PermissionInput::Remove { .. }
            | PermissionInput::RemoveDir { .. }
            | PermissionInput::Search
            | PermissionInput::None => {}
        }
    }

    /// Ship an add-directory request to the server.
    fn submit_add_dir(&mut self, path: String) {
        let Some(s) = self.sessions.current.connection.as_ref() else {
            self.system_line("permission: not connected");
            return;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("permission: request ids exhausted");
            return;
        };
        if let Err(e) = self.enqueue(ClientCommand::PermissionAddDirQuery { req_id, path }) {
            self.system_line(Self::enqueue_failure_line("permission", e));
            return;
        }
        self.system_line("permission: adding directory".to_string());
    }

    /// Ship a remove-directory request for the cursor-th entry of dirs_cache.
    pub(crate) fn remove_dir_at(&mut self, index: usize) {
        let Some(path) = self
            .sessions
            .current
            .capabilities
            .permission
            .dirs_cache
            .get(index)
            .cloned()
        else {
            self.system_line("permission: directory no longer present");
            return;
        };
        let Some(s) = self.sessions.current.connection.as_ref() else {
            self.system_line("permission: not connected");
            return;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("permission: request ids exhausted");
            return;
        };
        if let Err(e) = self.enqueue(ClientCommand::PermissionRemoveDirQuery { req_id, path }) {
            self.system_line(Self::enqueue_failure_line("permission", e));
            return;
        }
        self.system_line("permission: removing directory".to_string());
    }
}

/// Cycle a destination one step left (wrapping) in the visual order
/// [project user local]: project <- user <- local <- (wrap) project.
fn prev_destination(d: RuleDestination) -> RuleDestination {
    match d {
        RuleDestination::Project => RuleDestination::Local,
        RuleDestination::Local => RuleDestination::User,
        RuleDestination::User => RuleDestination::Project,
        _ => d,
    }
}

/// Cycle a destination one step right (wrapping) in the visual order
/// [project user local]: project -> user -> local -> (wrap) project.
fn next_destination(d: RuleDestination) -> RuleDestination {
    match d {
        RuleDestination::Project => RuleDestination::User,
        RuleDestination::User => RuleDestination::Local,
        RuleDestination::Local => RuleDestination::Project,
        _ => d,
    }
}

/// The one-line prompt shown above the rule list when a sub-mode is active.
pub(crate) fn permission_prompt(app: &App) -> Option<String> {
    match app.ui.permission_pane.input {
        PermissionInput::Add => Some(
            "add: <tool> [content:]<allow|ask|deny>  ·  e.g. bash npm:allow  ·  Enter to pick destination  ·  Esc cancel".into(),
        ),
        PermissionInput::AddDir => Some(
            "add directory: <path>  ·  Enter to add  ·  Esc cancel".into(),
        ),
        PermissionInput::AddDestination { destination, .. } => {
            let opts = destination_options_label(destination);
            Some(format!("destination:  {opts}  ·  ←→ pick  ·  Enter save  ·  Esc cancel"))
        }
        PermissionInput::Remove { confirm, .. } => {
            let opts = yes_no_label(confirm);
            Some(format!("remove this rule?  {opts}  ·  ←→ pick  ·  Enter  ·  Esc cancel"))
        }
        PermissionInput::RemoveDir { confirm, .. } => {
            let opts = yes_no_label(confirm);
            Some(format!("remove this directory?  {opts}  ·  ←→ pick  ·  Enter  ·  Esc cancel"))
        }
        PermissionInput::Search => Some("search: type to filter  ·  Enter done  ·  Esc clear".into()),
        PermissionInput::None => None,
    }
}

/// The destination options row: [project] user  local (the focused one
/// bracketed). Matches the tab-header bracket style.
fn destination_options_label(focused: RuleDestination) -> String {
    let parts: [(RuleDestination, &str); 3] = [
        (RuleDestination::Project, "project"),
        (RuleDestination::User, "user"),
        (RuleDestination::Local, "local"),
    ];
    parts
        .iter()
        .map(|(d, label)| {
            if *d == focused {
                format!("[{label}]")
            } else {
                format!(" {label} ")
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// The Yes/No confirm row: [No] Yes when confirm=false (default), [Yes] No
/// when true.
fn yes_no_label(confirm: bool) -> String {
    let (yes, no) = if confirm {
        ("[Yes]", " No ")
    } else {
        (" Yes ", "[No]")
    };
    format!("{yes}  {no}")
}

#[cfg(test)]
mod tests;
