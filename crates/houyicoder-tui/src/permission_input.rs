//! The /permissions pane input sub-modes: add a rule (spec text then a
//! destination pick), confirm a removal (Yes/No), and live search-filter the
//! current tab. None = list navigation. Search owns a pane-local buffer so it
//! never eats a slash command's leading slash; AddDestination and Remove are
//! Select-style (Left/Right + Enter) and own their keys. Add (spec text) is
//! the only sub-mode that falls through to the main input box for typing.

use houyicoder_protocol::frontend::permission::{
    PermissionEffect, PermissionRule, PermissionRuleContent, RuleDestination,
};

use crate::run_control::ClientCommand;
use crate::state::enums::CyclicTab;
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

/// /permission add <action> <effect>: parse the two args through the shared
/// spec parser (so bash npm:allow works from the command too) and ship.
pub(crate) fn add_rule_from_command(app: &mut App, action: &str, effect: &str) {
    let spec = match parse_rule_spec(&format!("{action} {effect}")) {
        Ok(s) => s,
        Err(msg) => {
            app.system_line(format!("permission: {msg}"));
            return;
        }
    };
    add_rule(app, spec);
}

/// Ship a parsed rule to the server with the default (project) destination.
/// Used by the /permission add command path. Refreshes arrive via
/// PermissionRulesResult.
fn add_rule(app: &mut App, spec: PermissionRuleSpec) {
    add_rule_with_destination(app, spec, RuleDestination::default());
}

/// Ship a parsed rule to the server with the chosen destination (persistence
/// scope). The wire rule carries the destination; the server projects it to
/// the engine Rule.scope at the boundary so the store writes the right file.
fn add_rule_with_destination(
    app: &mut App,
    spec: PermissionRuleSpec,
    destination: RuleDestination,
) {
    let wire_rule = PermissionRule {
        action: spec.action,
        content: spec.content,
        effect: spec.effect,
        destination,
    };
    let label = crate::command::render::permission_effect_label(spec.effect);
    let action = wire_rule.action.clone();
    let Some(req_id) = app.mint_request_id() else {
        app.system_line("permission: no server connected");
        return;
    };
    app.send_cmd(ClientCommand::PermissionAddRuleQuery {
        req_id,
        rule: wire_rule,
    });
    app.system_line(format!("permission: added {action} {label}"));
}

/// Remove the rule at the given index into the full rules_cache (the server
/// sees the unfiltered set, not the tab-filtered view).
pub(crate) fn remove_rule_at(app: &mut App, index: usize) {
    let Some(req_id) = app.mint_request_id() else {
        app.system_line("permission: no server connected");
        return;
    };
    app.send_cmd(ClientCommand::PermissionRemoveRuleQuery { req_id, index });
    app.system_line("permission: removed".to_string());
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

/// The live search query: the pane-local permission_search buffer while
/// Search is active, an empty string otherwise (so Add/Remove/nav never
/// filter the list).
pub(crate) fn search_query(app: &App) -> &str {
    if matches!(app.permission_input, PermissionInput::Search) {
        &app.permission_search
    } else {
        ""
    }
}

/// The index into rules_cache of the cursor-th filtered rule, for removal.
pub(crate) fn filtered_rule_index(app: &App) -> Option<usize> {
    if matches!(
        app.permission_tab,
        PermissionTab::Recent | PermissionTab::Workspace
    ) {
        return None;
    }
    let view = filtered_rules(app.permission_tab, &app.rules_cache, search_query(app));
    let cursor = app.permission_cursor.min(view.len().saturating_sub(1));
    let target = view.get(cursor).copied()? as *const PermissionRule;
    app.rules_cache.iter().position(|r| std::ptr::eq(r, target))
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
pub(crate) fn handle_permission_key(app: &mut App, k: ratatui::crossterm::event::KeyEvent) -> bool {
    use ratatui::crossterm::event::KeyCode;
    if app.pane != Pane::Permission {
        return false;
    }
    // Esc: leave a sub-mode first, then the pane.
    if k.code == KeyCode::Esc {
        if app.permission_input.is_active() {
            exit_input(app);
        } else {
            // Leaving the pane is a big layout transition; force a full
            // repaint so the ratatui diff against the prior pane frame does
            // not drop the transcript input box on the first redraw.
            app.pane = Pane::Transcript;
        }
        return true;
    }
    match app.permission_input {
        // Search owns its keystrokes in a pane-local buffer (return true) so
        // the main input box stays free for a slash command after Esc.
        PermissionInput::Search => handle_search_keys(app, k.code),
        // AddDestination + Remove + RemoveDir are Select-style: Left/Right
        // cycle, Enter fires. They own their keys so nothing leaks to input.
        PermissionInput::AddDestination { .. }
        | PermissionInput::Remove { .. }
        | PermissionInput::RemoveDir { .. } => handle_select_keys(app, k.code),
        // Add + AddDir (spec/dir text) fall through to the main input box.
        PermissionInput::Add | PermissionInput::AddDir => false,
        PermissionInput::None => handle_nav_keys(app, k.code),
    }
}

/// Select-style keys for the AddDestination and Remove-confirm steps:
/// Left/Right cycle the focused option, Enter commits, Backspace/other keys
/// are swallowed so the main input box is never touched.
fn handle_select_keys(app: &mut App, code: ratatui::crossterm::event::KeyCode) -> bool {
    use ratatui::crossterm::event::KeyCode;
    match &mut app.permission_input {
        PermissionInput::AddDestination { destination, .. } => match code {
            KeyCode::Left => {
                *destination = prev_destination(*destination);
                true
            }
            KeyCode::Right => {
                *destination = next_destination(*destination);
                true
            }
            KeyCode::Enter => {
                let (action, content, effect, destination) = match &app.permission_input {
                    PermissionInput::AddDestination {
                        action,
                        content,
                        effect,
                        destination,
                    } => (action.clone(), content.clone(), *effect, *destination),
                    _ => unreachable!(),
                };
                exit_input(app);
                add_rule_with_destination(
                    app,
                    PermissionRuleSpec {
                        action,
                        content,
                        effect,
                    },
                    destination,
                );
                true
            }
            _ => true,
        },
        PermissionInput::Remove { confirm, .. } => match code {
            KeyCode::Left | KeyCode::Right => {
                *confirm = !*confirm;
                true
            }
            KeyCode::Enter => {
                let (idx, confirm) = match &app.permission_input {
                    PermissionInput::Remove { idx, confirm } => (*idx, *confirm),
                    _ => unreachable!(),
                };
                exit_input(app);
                if confirm {
                    remove_rule_at(app, idx);
                } else {
                    app.system_line("permission: removal cancelled");
                }
                true
            }
            _ => true,
        },
        PermissionInput::RemoveDir { confirm, .. } => match code {
            KeyCode::Left | KeyCode::Right => {
                *confirm = !*confirm;
                true
            }
            KeyCode::Enter => {
                let (idx, confirm) = match &app.permission_input {
                    PermissionInput::RemoveDir { idx, confirm } => (*idx, *confirm),
                    _ => unreachable!(),
                };
                exit_input(app);
                if confirm {
                    remove_dir_at(app, idx);
                } else {
                    app.system_line("permission: directory removal cancelled");
                }
                true
            }
            _ => true,
        },
        _ => false,
    }
}

/// Cycle a destination one step left (wrapping) in the visual order
/// [project user local]: project <- user <- local <- (wrap) project. Session
/// and Builtin are not user-cyclable (they are internal scopes), so they stay.
fn prev_destination(d: RuleDestination) -> RuleDestination {
    match d {
        RuleDestination::Project => RuleDestination::Local,
        RuleDestination::Local => RuleDestination::User,
        RuleDestination::User => RuleDestination::Project,
        _ => d,
    }
}

/// Cycle a destination one step right (wrapping) in the visual order
/// [project user local]: project -> user -> local -> (wrap) project. Session
/// and Builtin stay (internal scopes, not cyclable in the Add flow).
fn next_destination(d: RuleDestination) -> RuleDestination {
    match d {
        RuleDestination::Project => RuleDestination::User,
        RuleDestination::User => RuleDestination::Local,
        RuleDestination::Local => RuleDestination::Project,
        _ => d,
    }
}

/// Search sub-mode keys: printable chars + Backspace edit the pane-local
/// permission_search buffer (live filter); Enter ends search (filter clears);
/// Up/Down navigate the filtered list. Everything else is swallowed.
fn handle_search_keys(app: &mut App, code: ratatui::crossterm::event::KeyCode) -> bool {
    use ratatui::crossterm::event::KeyCode;
    match code {
        KeyCode::Char(c) => {
            app.permission_search.push(c);
            app.permission_cursor = 0;
            true
        }
        KeyCode::Backspace => {
            app.permission_search.pop();
            app.permission_cursor = 0;
            true
        }
        KeyCode::Enter => {
            exit_input(app);
            true
        }
        KeyCode::Up => {
            app.permission_cursor = app.permission_cursor.saturating_sub(1);
            true
        }
        KeyCode::Down => {
            app.permission_cursor = app.permission_cursor.saturating_add(1);
            true
        }
        _ => true,
    }
}

/// List navigation + sub-mode entry keys (no sub-mode active). 'a' add, 'd'
/// remove (on a focused rule), 's' search; Left/Right cycle tabs; Up/Down move
/// the cursor. Entry keys fire only on an empty input + a rule tab.
fn handle_nav_keys(app: &mut App, code: ratatui::crossterm::event::KeyCode) -> bool {
    use ratatui::crossterm::event::KeyCode;
    let empty = app.input.is_empty();
    let on_rule_tab = matches!(
        app.permission_tab,
        PermissionTab::Allow | PermissionTab::Ask | PermissionTab::Deny
    );
    match code {
        KeyCode::Left => {
            app.permission_tab = app.permission_tab.prev();
            app.permission_cursor = 0;
            true
        }
        KeyCode::Right => {
            app.permission_tab = app.permission_tab.next();
            app.permission_cursor = 0;
            true
        }
        KeyCode::Up => {
            app.permission_cursor = app.permission_cursor.saturating_sub(1);
            true
        }
        KeyCode::Down => {
            app.permission_cursor = app.permission_cursor.saturating_add(1);
            true
        }
        KeyCode::Char('a') if empty && on_rule_tab => {
            app.permission_input = PermissionInput::Add;
            app.input.clear();
            true
        }
        KeyCode::Char('a') if empty && app.permission_tab == PermissionTab::Workspace => {
            // Add-directory flow: type a path in the main input box, Enter
            // ships it. The server canonicalizes + extends the fence; the
            // PermissionWorkingDirsResult ack refreshes dirs_cache.
            app.permission_input = PermissionInput::AddDir;
            app.input.clear();
            true
        }
        KeyCode::Char('d') if empty && on_rule_tab => {
            // Enter Remove with No preselected (confirm=false): the user must
            // move to Yes + Enter to delete, so a stray Enter never removes.
            if let Some(idx) = filtered_rule_index(app) {
                app.permission_input = PermissionInput::Remove {
                    idx,
                    confirm: false,
                };
                app.input.clear();
            }
            true
        }
        KeyCode::Char('d') if empty && app.permission_tab == PermissionTab::Workspace => {
            // Remove the cursor-selected directory. No preselected (confirm=
            // false) so a stray Enter cannot drop a dir. The cursor indexes
            // into dirs_cache; saturating-clamped at render + ship time.
            let idx = app
                .permission_cursor
                .min(app.dirs_cache.len().saturating_sub(1));
            if !app.dirs_cache.is_empty() {
                app.permission_input = PermissionInput::RemoveDir {
                    idx,
                    confirm: false,
                };
                app.input.clear();
            }
            true
        }
        KeyCode::Char('s') if empty && on_rule_tab => {
            app.permission_input = PermissionInput::Search;
            app.permission_search.clear();
            app.permission_cursor = 0;
            true
        }
        _ => false,
    }
}

/// Leave any sub-mode and clear the input box.
fn exit_input(app: &mut App) {
    app.permission_input = PermissionInput::None;
    app.input.clear();
    app.permission_search.clear();
}

/// Submit the Add / AddDir sub-mode's typed text (called from submit_input
/// when Enter falls through). Add parses the spec and advances to the
/// destination pick; AddDir ships the path straight to the server. The
/// other sub-modes (AddDestination, Remove, RemoveDir, Search) own their
/// own Enter in handle_permission_key and never reach here.
pub(crate) fn submit_permission_input(app: &mut App, text: String) {
    match app.permission_input.clone() {
        PermissionInput::Add => match parse_rule_spec(&text) {
            Ok(spec) => {
                // Advance to the destination pick (default project); carry the
                // parsed spec as pub protocol types so the enum stays
                // externally nameable. Clear the input so it is free for the
                // Select's Left/Right/Enter.
                app.input.clear();
                app.permission_input = PermissionInput::AddDestination {
                    action: spec.action,
                    content: spec.content,
                    effect: spec.effect,
                    destination: RuleDestination::default(),
                };
            }
            Err(msg) => {
                app.system_line(format!("permission: {msg}"));
                exit_input(app);
            }
        },
        PermissionInput::AddDir => {
            let path = text.trim().to_string();
            exit_input(app);
            if path.is_empty() {
                app.system_line("permission: empty directory path");
            } else {
                submit_add_dir(app, path);
            }
        }
        // AddDestination/Remove/RemoveDir/Search handle Enter themselves; this
        // is a no-op safety net (should not be reached).
        PermissionInput::AddDestination { .. }
        | PermissionInput::Remove { .. }
        | PermissionInput::RemoveDir { .. }
        | PermissionInput::Search
        | PermissionInput::None => {}
    }
}

/// Ship an add-directory request to the server. The server canonicalizes +
/// validates the path is a directory and extends the fence; the ack
/// (PermissionWorkingDirsResult) refreshes dirs_cache. A None req_id (no
/// session) surfaces a system line instead of panicking.
fn submit_add_dir(app: &mut App, path: String) {
    let Some(req_id) = app.mint_request_id() else {
        app.system_line("permission: no server connected");
        return;
    };
    app.send_cmd(ClientCommand::PermissionAddWorkingDirQuery { req_id, path });
    app.system_line("permission: adding directory".to_string());
}

/// Ship a remove-directory request for the cursor-th entry of dirs_cache.
/// Bounds-checked against the live cache; a stale cursor (dirs changed since
/// the sub-mode entered) is a no-op with a system line.
fn remove_dir_at(app: &mut App, index: usize) {
    let Some(path) = app.dirs_cache.get(index).cloned() else {
        app.system_line("permission: directory no longer present");
        return;
    };
    let Some(req_id) = app.mint_request_id() else {
        app.system_line("permission: no server connected");
        return;
    };
    app.send_cmd(ClientCommand::PermissionRemoveWorkingDirQuery { req_id, path });
    app.system_line("permission: removing directory".to_string());
}

/// The one-line prompt shown above the rule list when a sub-mode is active.
pub(crate) fn permission_prompt(app: &App) -> Option<String> {
    match app.permission_input {
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
