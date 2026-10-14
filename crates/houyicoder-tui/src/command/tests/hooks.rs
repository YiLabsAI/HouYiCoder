//! The /hooks command: its sub-commands and the pane it opens.

use houyicoder_protocol::envelope::RequestId;

use crate::test_harness::{render_text, working_app};

/// /hooks opens the Hooks pane (a live view, not a transcript system line) +
/// requests the hook list when a runner is connected.
#[test]
fn test_hooks_opens_pane_connected() {
    let mut app = build_app_for_test(None);
    app.run_command(SlashCommand::Hooks);
    assert_eq!(app.ui.navigation.pane, Pane::Hooks, "/hooks opens the pane");
}

/// /hooks with no runner connected still opens the pane (shows the empty hook
/// list row).
#[test]
fn test_opens_pane_no_runner() {
    let mut app = working_app();
    app.run_command(SlashCommand::Hooks);
    assert_eq!(app.ui.navigation.pane, Pane::Hooks);
    assert!(app.sessions.current.capabilities.hooks.entries.is_empty());
}

/// A HooksResult populates the hooks capability cache (a live view).
#[test]
fn test_hooks_result_stores_entries() {
    let mut app = working_app();
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(1),
        response: ServerResponse::Hooks {
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
        },
    });
    assert_eq!(app.sessions.current.capabilities.hooks.entries.len(), 2);
    assert_eq!(
        app.sessions.current.capabilities.hooks.entries[0].name,
        "pre-check"
    );
}

/// The /hooks pane journey: with framework + configured entries, the list
/// renders the "N hooks configured" subtitle (N = non-framework hooks), a
/// count on configured events, configured events sorted first, and the
/// detail view (level 1) shows the event description + the settings.json
/// hint. Covers the display surface end-to-end at the render level; the
/// open/Esc cases are covered by the PTY layer in tests/ui_hooks.
#[test]
fn test_hooks_pane_list_detail() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Hooks;
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(2),
        response: ServerResponse::Hooks {
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
        },
    });
    let out = render_text(&app, 100, 28);
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
    app.ui.hooks_pane.level.set(1);
    app.ui.hooks_pane.sel.set(0);
    let detail = render_text(&app, 100, 28);
    assert!(
        detail.contains("Input to command is JSON"),
        "detail shows the event description: {detail}"
    );
    assert!(
        detail.contains("edit settings.json"),
        "detail shows the settings.json hint: {detail}"
    );
}

use crate::agent_message::{ServerResponse, SessionMessage};
use crate::composition::build_app_for_test;
use crate::keys::handle_working;
use crate::state::Pane;
/// /hooks pane navigation keys: level-0 Esc exits to the transcript, Up/Down
/// move the cursor, Enter drills into the detail level. These arms had no
/// unit coverage before (only the PTY layer exercised them).
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::frontend::SlashCommand;
use houyicoder_protocol::frontend::hooks::HookEntry;
fn hkey(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

#[test]
fn test_hooks_esc_to_list() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Hooks;
    app.ui.hooks_pane.level.set(1);
    handle_working(&mut app, hkey(KeyCode::Esc));
    assert_eq!(
        app.ui.hooks_pane.level.get(),
        0,
        "Esc at level 1 returns to the list"
    );
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Hooks,
        "stays in the hooks pane"
    );
}

#[test]
fn test_hooks_esc_exits() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Hooks;
    app.ui.hooks_pane.level.set(0);
    handle_working(&mut app, hkey(KeyCode::Esc));
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Transcript,
        "Esc at level 0 exits"
    );
}

#[test]
fn test_hooks_up_moves() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Hooks;
    app.ui.hooks_pane.level.set(0);
    app.ui.hooks_pane.sel.set(2);
    handle_working(&mut app, hkey(KeyCode::Up));
    assert_eq!(app.ui.hooks_pane.sel.get(), 1, "Up moves the cursor back");
}

#[test]
fn test_hooks_down_clamps() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Hooks;
    app.ui.hooks_pane.level.set(0);
    app.ui.hooks_pane.sel.set(0);
    handle_working(&mut app, hkey(KeyCode::Down));
    assert_eq!(
        app.ui.hooks_pane.sel.get(),
        0,
        "Down clamps at 0 with no entries"
    );
}

#[test]
fn test_hooks_enter_detail() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Hooks;
    app.ui.hooks_pane.level.set(0);
    handle_working(&mut app, hkey(KeyCode::Enter));
    assert_eq!(
        app.ui.hooks_pane.level.get(),
        1,
        "Enter drills into the detail level"
    );
}
