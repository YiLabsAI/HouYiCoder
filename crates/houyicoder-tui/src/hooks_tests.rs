//! Hooks command tests split from scroll_tests.rs for file-size.

use super::working;

/// /hooks opens the Hooks pane (a live view, not a transcript system line) +
/// requests the hook list when a runner is wired.
#[test]
fn test_hooks_opens_pane_wired() {
    let mut app = crate::composition::build_app_for_test(None);
    app.run_command(houyicoder_protocol::frontend::SlashCommand::Hooks);
    assert_eq!(app.pane, crate::state::Pane::Hooks, "/hooks opens the pane");
}

/// /hooks with no runner wired still opens the pane (shows the empty hook
/// list row).
#[test]
fn test_opens_pane_no_runner() {
    let mut app = working();
    app.run_command(houyicoder_protocol::frontend::SlashCommand::Hooks);
    assert_eq!(app.pane, crate::state::Pane::Hooks);
    assert!(app.hook_entries.is_empty());
}

/// A HooksResult populates the pane's hook_entries cache (a live view).
#[test]
fn test_hooks_result_stores_entries() {
    use crate::run_control::AgentMessage;
    use houyicoder_protocol::frontend::hooks::HookEntry;
    let mut app = working();
    app.handle_agent_message(AgentMessage::HooksResult {
        hooks: vec![
            HookEntry {
                name: "pre-check".into(),
                events: vec!["PreToolUse".into()],
                source: "Project".into(),
                fired: true,
                summary: String::new(),
                description: String::new(),
            },
            HookEntry {
                name: "PreToolUse".into(),
                events: vec!["PreToolUse".into()],
                source: "framework".into(),
                fired: true,
                summary: "Before tool execution".into(),
                description: String::new(),
            },
        ],
    });
    assert_eq!(app.hook_entries.len(), 2);
    assert_eq!(app.hook_entries[0].name, "pre-check");
}

/// The /hooks pane journey: with framework + configured entries, the list
/// renders the "N hooks configured" subtitle (N = non-framework hooks), a
/// count on configured events, configured events sorted first, and the
/// detail view (level 1) shows the event description + the settings.json
/// hint. Covers the display surface end-to-end at the render level; the
/// open/Esc cases are covered by the PTY layer in tests/ui_hooks.
#[test]
fn test_hooks_pane_list_detail() {
    use crate::run_control::AgentMessage;
    use houyicoder_protocol::frontend::hooks::HookEntry;
    let mut app = working();
    app.pane = crate::state::Pane::Hooks;
    app.handle_agent_message(AgentMessage::HooksResult {
        hooks: vec![
            HookEntry {
                name: "pre-check".into(),
                events: vec!["PreToolUse".into()],
                source: "Project".into(),
                fired: true,
                summary: String::new(),
                description: String::new(),
            },
            HookEntry {
                name: "PreToolUse".into(),
                events: vec!["PreToolUse".into()],
                source: "framework".into(),
                fired: true,
                summary: "Before tool execution".into(),
                description: "Input to command is JSON".into(),
            },
            HookEntry {
                name: "PostToolUse".into(),
                events: vec!["PostToolUse".into()],
                source: "framework".into(),
                fired: false,
                summary: "After tool execution".into(),
                description: String::new(),
            },
        ],
    });
    let out = super::render(&app);
    assert!(
        out.contains("1 hooks configured"),
        "subtitle counts configured (non-framework) hooks: {out}"
    );
    assert!(out.contains("PreToolUse"), "configured event listed: {out}");
    assert!(
        out.contains("(1)"),
        "configured event shows its count: {out}"
    );
    let pre = out.find("PreToolUse");
    let post = out.find("PostToolUse");
    if let (Some(p), Some(pt)) = (pre, post) {
        assert!(p < pt, "configured sorts before unconfigured: {out}");
    }
    app.hooks_level.set(1);
    app.hooks_sel.set(0);
    let detail = super::render(&app);
    assert!(
        detail.contains("Input to command is JSON"),
        "detail shows the event description: {detail}"
    );
    assert!(
        detail.contains("edit settings.json"),
        "detail shows the settings.json hint: {detail}"
    );
}

/// /hooks pane navigation keys: level-0 Esc exits to the transcript, Up/Down
/// move the cursor, Enter drills into the detail level. These arms had no
/// unit coverage before (only the PTY layer exercised them).
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
fn hkey(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

#[test]
fn test_hooks_esc_to_list() {
    let mut app = working();
    app.pane = crate::state::Pane::Hooks;
    app.hooks_level.set(1);
    crate::keys::handle_working(&mut app, hkey(KeyCode::Esc));
    assert_eq!(
        app.hooks_level.get(),
        0,
        "Esc at level 1 returns to the list"
    );
    assert_eq!(
        app.pane,
        crate::state::Pane::Hooks,
        "stays in the hooks pane"
    );
}

#[test]
fn test_hooks_esc_exits() {
    let mut app = working();
    app.pane = crate::state::Pane::Hooks;
    app.hooks_level.set(0);
    crate::keys::handle_working(&mut app, hkey(KeyCode::Esc));
    assert_eq!(
        app.pane,
        crate::state::Pane::Transcript,
        "Esc at level 0 exits"
    );
}

#[test]
fn test_hooks_up_moves() {
    let mut app = working();
    app.pane = crate::state::Pane::Hooks;
    app.hooks_level.set(0);
    app.hooks_sel.set(2);
    crate::keys::handle_working(&mut app, hkey(KeyCode::Up));
    assert_eq!(app.hooks_sel.get(), 1, "Up moves the cursor back");
}

#[test]
fn test_hooks_down_clamps() {
    let mut app = working();
    app.pane = crate::state::Pane::Hooks;
    app.hooks_level.set(0);
    app.hooks_sel.set(0);
    crate::keys::handle_working(&mut app, hkey(KeyCode::Down));
    assert_eq!(app.hooks_sel.get(), 0, "Down clamps at 0 with no entries");
}

#[test]
fn test_hooks_enter_detail() {
    let mut app = working();
    app.pane = crate::state::Pane::Hooks;
    app.hooks_level.set(0);
    crate::keys::handle_working(&mut app, hkey(KeyCode::Enter));
    assert_eq!(
        app.hooks_level.get(),
        1,
        "Enter drills into the detail level"
    );
}
