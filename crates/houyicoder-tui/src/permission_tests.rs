//! Integration tests for the permission mode loop: the Shift+Tab cycle and
//! the /permissions rule manager. These drive the real App command path (no
//! runner needed — /permissions is a TUI-local command that works regardless
//! of wiring; mode switching is Shift+Tab, covered here at the state layer +
//! by the PTY ui_mode binary at the real-terminal layer).

#![cfg(test)]

use crate::composition;
use crate::state::{App, Pane};
use crate::test_support::render_text;

fn app() -> App {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app.pane = Pane::Transcript;
    app
}

#[test]
fn test_reasoning_no_live_block() {
    // Live reasoning streams into live_reasoning_text but does NOT render a
    // ∴ Thinking block during the turn — the thinking indicator is the
    // spinner row's verb (live reasoning is not
    // echoed as a block; the ∴ block is default-hidden, verbose only).
    // Echoing live reasoning each frame was a self-invented surplus that
    // surfaced a ctrl+o hint on every interaction.
    let mut app = app();
    app.handle_agent_message(crate::run_control::AgentMessage::ReasoningDelta {
        text: "pondering deeply".into(),
    });
    assert_eq!(app.live_reasoning_text, "pondering deeply");
    assert!(app.live_active);
    let text = render_text(&app, 80, 24);
    assert!(
        !text.contains("∴ Thinking"),
        "no live ∴ Thinking block during the turn:\n{text}"
    );
    assert!(
        !text.contains("pondering"),
        "live reasoning content must not echo into the transcript:\n{text}"
    );
}

fn run(app: &mut App, cmd: &str) {
    app.input.set(cmd.to_string());
    app.submit_input();
}

fn last_system(app: &App) -> String {
    app.transcript
        .iter()
        .rev()
        .find_map(|l| match l {
            crate::state::TranscriptLine::System(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

#[test]
fn test_permission_rule_add_list() {
    // /permissions add/list/del are wire verbs now (server authority). Drive
    // them on a wired app + pump the driver between commands so the server's
    // PermissionRulesResult replies land in rules_cache before the next read.
    let mut app = crate::composition::build_app_for_test(None);
    app.screen = crate::state::Screen::Working;
    app.pane = Pane::Transcript;
    run(&mut app, "/permissions add bash allow");
    assert!(last_system(&app).contains("added"));
    pump_rules(&mut app);
    run(&mut app, "/permissions list");
    assert!(last_system(&app).contains("bash"));
    assert!(last_system(&app).contains("allow"));
    run(&mut app, "/permissions del 0");
    assert!(last_system(&app).contains("removed"));
    pump_rules(&mut app);
    run(&mut app, "/permissions list");
    assert!(last_system(&app).contains("no rules"));
}

/// Pump the driver until the rules cache length changes (the add/del replies
/// ship a fresh rule set), or the wait times out.
fn pump_rules(app: &mut App) {
    let mark = app.rules_cache.len();
    for _ in 0..200 {
        app.poll_agent();
        if app.rules_cache.len() != mark {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn test_permission_view_shows_mode() {
    let mut app = app();
    run(&mut app, "/permissions view");
    let out = last_system(&app);
    assert!(out.contains("mode: auto"), "{out}");
    assert!(out.contains("rules:"), "{out}");
    assert!(out.contains("verdicts:"), "{out}");
}

#[test]
fn test_undo_no_shows_message() {
    let mut app = app();
    run(&mut app, "/undo");
    assert!(last_system(&app).contains("no server connected"));
}

#[test]
fn test_permission_bad_effect_rejected() {
    let mut app = app();
    run(&mut app, "/permissions add bash bogus");
    assert!(last_system(&app).contains("permission:"));
    assert_eq!(app.rules_cache.len(), 0);
}

#[test]
fn test_status_bar_renders_mode() {
    // A wired app shows the mode pill in the agent status bar. Render and
    // confirm the current mode label lands in the bottom status line.
    let mut app = crate::composition::build_app_for_test(None);
    app.screen = crate::state::Screen::Working;
    app.mode_cache = Some(houyicoder_protocol::frontend::permission::PermissionMode::Auto);
    let text = render_text(&app, 100, 28);
    assert!(
        text.contains("auto mode on"),
        "status bar should show the mode pill, got:\n{text}"
    );
}

#[test]
fn test_status_bar_renders_manual() {
    // Manual mode shows the pause-glyph pill in the status bar.
    let mut app = crate::composition::build_app_for_test(None);
    app.screen = crate::state::Screen::Working;
    app.mode_cache = Some(houyicoder_protocol::frontend::permission::PermissionMode::Manual);
    let text = render_text(&app, 100, 28);
    assert!(
        text.contains("manual mode on"),
        "status bar should show the manual pill, got:\n{text}"
    );
}

#[test]
fn test_approval_popup_no_placeholder() {
    let mut app = crate::composition::build_app_for_test(None);
    app.screen = crate::state::Screen::Working;
    app.approval = Some(crate::state::Approval {
        tool: "bash".into(),
        args: "{\"command\":\"ls\"}".into(),
        reason: "agent wants to run this tool".into(),
        selected: 0,
        call_id: "c1".into(),
        options: Vec::new(),
        ..Default::default()
    });
    let text = render_text(&app, 80, 24);
    // Inline prompt renders the real tool call (no placeholder); the proceed
    // question and Esc hint are present.
    assert!(
        text.contains("Bash command"),
        "tool header missing:\n{text}"
    );
    assert!(
        !text.contains("(placeholder)"),
        "placeholder still in prompt:\n{text}"
    );
    assert!(
        text.contains("Do you want to proceed?"),
        "proceed question missing:\n{text}"
    );
    assert!(text.contains("Esc cancel"), "Esc hint missing:\n{text}");
}

#[test]
fn test_approval_r_binds_reject() {
    // Safety: the hint says r=reject. Pressing r must focus reject (selected=1),
    // not fall through and leave selected=0 (approve) — else r+Enter silently
    // approves. Regression guard for the r-key binding.
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = crate::composition::build_app_for_test(None);
    app.screen = crate::state::Screen::Working;
    app.approval = Some(crate::state::Approval {
        tool: "bash".into(),
        args: "{\"command\":\"rm -rf /\"}".into(),
        reason: "dangerous".into(),
        selected: 0,
        call_id: "c1".into(),
        options: Vec::new(),
        ..Default::default()
    });
    let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
    // handle_working dispatches to handle_approval when approval is set.
    crate::keys::handle_working(&mut app, key);
    assert_eq!(
        app.approval.as_ref().expect("approval").selected,
        1,
        "r must focus reject, not leave approve selected"
    );
}

#[test]
fn test_git_ops_toggle_command() {
    // /permissions git off flips the cached toggle (optimistic without a
    // server); /permissions git shows the state. Drives the real command path.
    let mut app = app();
    assert!(app.ask_before_git_enabled, "default on");
    app.input.set("/permissions git off".to_string());
    app.submit_input();
    assert!(
        !app.ask_before_git_enabled,
        "off after /permissions git off"
    );
    app.input.set("/permissions git on".to_string());
    app.submit_input();
    assert!(app.ask_before_git_enabled, "on after /permissions git on");
}

#[test]
fn test_git_ops_show_command() {
    // /permissions git (no on/off) reports the current toggle state. Without a
    // server it reads the cache; the line names the git operations it governs.
    let mut app = app();
    app.input.set("/permissions git".to_string());
    app.submit_input();
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, crate::state::TranscriptLine::System(s) if s.contains("ask before git operations: on"))),
        "bare /permissions git shows the on state"
    );
}

#[test]
fn test_git_ops_updates_cache() {
    // The server's PermissionAskBeforeGitResult refreshes the cache so the
    // /permissions view reflects the gate's authoritative state.
    let mut app = app();
    app.handle_agent_message(
        crate::agent_message::AgentMessage::PermissionAskBeforeGitResult { enabled: false },
    );
    assert!(!app.ask_before_git_enabled);
    app.handle_agent_message(
        crate::agent_message::AgentMessage::PermissionAskBeforeGitResult { enabled: true },
    );
    assert!(app.ask_before_git_enabled);
}

#[test]
fn test_permission_pane_esc_exits() {
    // Bare /permissions opens the interactive pane; Esc exits back to the
    // transcript.
    use crate::state::Pane;
    let mut app = app();
    app.input.set("/permissions".to_string());
    app.submit_input();
    assert_eq!(
        app.pane,
        Pane::Permission,
        "bare /permissions opens the pane"
    );
    // Esc exits back to the transcript.
    crate::keys::handle_working(
        &mut app,
        ratatui::crossterm::event::KeyEvent {
            code: ratatui::crossterm::event::KeyCode::Esc,
            modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
            kind: ratatui::crossterm::event::KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        },
    );
    assert_eq!(app.pane, Pane::Transcript, "Esc exits the permission pane");
}

#[test]
fn test_open_no_transcript_hint() {
    // Opening the pane is a view switch, not a turn that needs an in-stream
    // acknowledgment. The pane owns its own footer hint, so /permissions must
    // not echo a "rule manager ..." system line into the transcript.
    use crate::state::Pane;
    let mut app = app();
    app.input.set("/permissions".to_string());
    app.submit_input();
    assert_eq!(app.pane, Pane::Permission);
    let systems: Vec<String> = app
        .transcript
        .iter()
        .rev()
        .filter_map(|l| match l {
            crate::state::TranscriptLine::System(s) => Some(s.clone()),
            _ => None,
        })
        .take(2)
        .collect();
    assert!(
        !systems.iter().any(|s| s.contains("rule manager")),
        "pane open must not echo a system hint line: {systems:?}"
    );
}

#[test]
fn test_permission_pane_renders_tabs() {
    // The pane shows the five tab labels with the active one bracketed, plus the
    // footer hint. Active tab defaults to Allow.
    use crate::state::PermissionTab;
    let mut app = app();
    app.pane = Pane::Permission;
    let text = render_text(&app, 80, 24);
    assert!(text.contains("Allow"), "tab header lists Allow");
    assert!(text.contains("Ask"), "tab header lists Ask");
    assert!(text.contains("Deny"), "tab header lists Deny");
    assert!(
        text.contains("Recently denied"),
        "tab header lists Recently denied"
    );
    assert!(text.contains("Workspace"), "tab header lists Workspace");
    assert!(text.contains("[Allow]"), "active tab is bracketed");
    assert!(
        text.contains("Esc to cancel"),
        "rule-tab footer shows the cancel hint: {text}"
    );
    assert_eq!(app.permission_tab, PermissionTab::Allow);
}

#[test]
fn test_pane_tab_cycle_wraps() {
    // Right cycles Allow -> Ask -> Deny -> Workspace -> Recently denied -> Allow.
    // Tab order is denials-first, workspace-last.
    use crate::state::PermissionTab;
    let mut app = app();
    app.pane = Pane::Permission;
    let key = |code: ratatui::crossterm::event::KeyCode| ratatui::crossterm::event::KeyEvent {
        code,
        modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
        kind: ratatui::crossterm::event::KeyEventKind::Press,
        state: ratatui::crossterm::event::KeyEventState::NONE,
    };
    crate::keys::handle_working(&mut app, key(ratatui::crossterm::event::KeyCode::Right));
    assert_eq!(app.permission_tab, PermissionTab::Ask);
    crate::keys::handle_working(&mut app, key(ratatui::crossterm::event::KeyCode::Right));
    assert_eq!(app.permission_tab, PermissionTab::Deny);
    crate::keys::handle_working(&mut app, key(ratatui::crossterm::event::KeyCode::Right));
    assert_eq!(app.permission_tab, PermissionTab::Workspace);
    crate::keys::handle_working(&mut app, key(ratatui::crossterm::event::KeyCode::Right));
    assert_eq!(app.permission_tab, PermissionTab::Recent);
    crate::keys::handle_working(&mut app, key(ratatui::crossterm::event::KeyCode::Right));
    assert_eq!(app.permission_tab, PermissionTab::Allow, "wraps to Allow");
    // Left goes the other way: Allow -> Recently denied.
    crate::keys::handle_working(&mut app, key(ratatui::crossterm::event::KeyCode::Left));
    assert_eq!(app.permission_tab, PermissionTab::Recent);
}

fn permission_ignores_unknown_key() {
    // A non-nav key in the permission pane falls through (no state change) so
    // the rest of the working keyset can handle it.
    let mut app = app();
    app.pane = Pane::Permission;
    let before_tab = app.permission_tab;
    let before_cursor = app.permission_cursor;
    crate::keys::handle_working(
        &mut app,
        ratatui::crossterm::event::KeyEvent {
            code: ratatui::crossterm::event::KeyCode::Char('z'),
            modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
            kind: ratatui::crossterm::event::KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        },
    );
    assert_eq!(app.pane, Pane::Permission, "pane unchanged");
    assert_eq!(app.permission_tab, before_tab, "tab unchanged");
    assert_eq!(app.permission_cursor, before_cursor, "cursor unchanged");
}

#[test]
fn test_permission_slash_blocked_browsing() {
    // When the permission pane is open in browsing mode (no Add/AddDir
    // sub-mode active), the input box is hidden and all input keys are
    // swallowed — including '/'. The user must Esc close the pane first,
    // then '/' to open the palette. This matches the other panes (Model,
    // Status, Memory, etc.) — a uniform pane-input rule.
    let mut app = app();
    app.pane = Pane::Permission;
    app.permission_input = crate::state::PermissionInput::None;
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Char('/')),
    );
    assert_eq!(
        app.permission_input,
        crate::state::PermissionInput::None,
        "'/' must not enter the search sub-mode"
    );
    assert!(
        !app.palette.open,
        "'/' swallowed in browse mode, no palette behind pane"
    );
}

fn key_press(code: ratatui::crossterm::event::KeyCode) -> ratatui::crossterm::event::KeyEvent {
    ratatui::crossterm::event::KeyEvent {
        code,
        modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
        kind: ratatui::crossterm::event::KeyEventKind::Press,
        state: ratatui::crossterm::event::KeyEventState::NONE,
    }
}

#[test]
fn test_permission_add_submode_flow() {
    // a enters Add, typing fills the input, Enter parses + ships the rule,
    // sub-mode clears. (In wired mode the pane does not render; asserts state.)
    use crate::state::PermissionInput;
    let mut app = crate::composition::build_app_for_test(None);
    app.screen = crate::state::Screen::Working;
    app.pane = Pane::Permission;
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Char('a')),
    );
    assert_eq!(app.permission_input, PermissionInput::Add);
    for c in "bash npm:allow".chars() {
        crate::keys::handle_working(
            &mut app,
            key_press(ratatui::crossterm::event::KeyCode::Char(c)),
        );
    }
    assert_eq!(app.input.value(), "bash npm:allow");
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Enter),
    );
    // Enter on the spec advances to the destination pick (default project),
    // not a ship — the Add flow is two steps now.
    use houyicoder_protocol::frontend::permission::RuleDestination;
    assert!(
        matches!(
            app.permission_input,
            PermissionInput::AddDestination {
                destination: RuleDestination::Project,
                ..
            }
        ),
        "Add spec Enter advances to destination pick: {:?}",
        app.permission_input
    );
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Enter),
    );
    assert_eq!(
        app.permission_input,
        PermissionInput::None,
        "destination Enter ships + clears sub-mode"
    );
    assert!(app.input.is_empty(), "input cleared after submit");
    pump_rules(&mut app);
    let added = app.rules_cache.iter().any(|r| {
        r.action == "bash"
            && matches!(
                r.effect,
                houyicoder_protocol::frontend::permission::PermissionEffect::Allow
            )
    });
    assert!(added, "rule landed in cache: {:?}", app.rules_cache);
}

#[test]
fn test_permission_add_prompt_renders() {
    // Stub app (no runner) renders the pane; the Add sub-mode shows its prompt.
    use crate::state::PermissionInput;
    let mut app = app();
    app.pane = Pane::Permission;
    app.permission_input = PermissionInput::Add;
    let text = render_text(&app, 100, 28);
    assert!(text.contains("add:"), "Add prompt renders: {text}");
    app.permission_input = PermissionInput::Remove {
        idx: 0,
        confirm: false,
    };
    let text = render_text(&app, 100, 28);
    assert!(
        text.contains("remove this rule"),
        "Remove prompt renders: {text}"
    );
    app.permission_input = PermissionInput::Search;
    let text = render_text(&app, 100, 28);
    assert!(
        text.contains("Search"),
        "SearchBox renders with the placeholder: {text}"
    );
}

#[test]
fn test_permission_remove_submode_deletes() {
    // 'd' on a focused rule enters Remove (No preselected); Right moves to
    // Yes, Enter ships the removal. Covers the Remove confirm + the ptr-eq
    // index resolution.
    use crate::state::PermissionInput;
    let mut app = crate::composition::build_app_for_test(None);
    app.screen = crate::state::Screen::Working;
    app.pane = Pane::Permission;
    run(&mut app, "/permissions add bash allow");
    pump_rules(&mut app);
    assert!(!app.rules_cache.is_empty());
    // Cursor sits on the only rule; 'd' enters Remove (No preselected).
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Char('d')),
    );
    assert!(matches!(
        app.permission_input,
        PermissionInput::Remove { confirm: false, .. }
    ));
    // Right moves to Yes; Enter confirms + ships.
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Right),
    );
    assert!(matches!(
        app.permission_input,
        PermissionInput::Remove { confirm: true, .. }
    ));
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Enter),
    );
    assert_eq!(
        app.permission_input,
        PermissionInput::None,
        "Remove Enter clears sub-mode"
    );
    pump_rules(&mut app);
    assert!(
        app.rules_cache.is_empty(),
        "rule removed: {:?}",
        app.rules_cache
    );
}

#[test]
fn test_permission_workspace_add_flow() {
    // 'a' on the Workspace tab enters AddDir; typing fills the input box;
    // Enter ships the path (the system line confirms the ship). Uses the
    // wired app so typed chars route to the input box like the rule Add flow.
    use crate::state::{PermissionInput, PermissionTab};
    let mut app = crate::composition::build_app_for_test(None);
    app.screen = crate::state::Screen::Working;
    app.pane = Pane::Permission;
    app.permission_tab = PermissionTab::Workspace;
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Char('a')),
    );
    assert_eq!(
        app.permission_input,
        PermissionInput::AddDir,
        "'a' on Workspace enters AddDir"
    );
    for c in "/tmp/extra".chars() {
        crate::keys::handle_working(
            &mut app,
            key_press(ratatui::crossterm::event::KeyCode::Char(c)),
        );
    }
    assert_eq!(app.input.value(), "/tmp/extra");
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Enter),
    );
    assert_eq!(
        app.permission_input,
        PermissionInput::None,
        "AddDir Enter clears the sub-mode"
    );
    let sys = last_system(&app);
    assert!(
        sys.contains("adding") || sys.contains("no server connected"),
        "AddDir surfaces a system line: {sys}"
    );
}

#[test]
fn test_permission_workspace_remove_flow() {
    // 'd' on a Workspace cursor dir enters RemoveDir (No preselected); Right
    // moves to Yes; Enter ships the removal. Stub app → mint None → system
    // line; the sub-mode clears. Covers the Remove directory confirm flow.
    use crate::state::{PermissionInput, PermissionTab};
    let mut app = app();
    app.pane = Pane::Permission;
    app.permission_tab = PermissionTab::Workspace;
    app.dirs_cache = vec!["/tmp/extra".into()];
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Char('d')),
    );
    assert!(
        matches!(
            app.permission_input,
            PermissionInput::RemoveDir { confirm: false, .. }
        ),
        "'d' on a dir enters RemoveDir (No preselected)"
    );
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Right),
    );
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Enter),
    );
    assert_eq!(
        app.permission_input,
        PermissionInput::None,
        "RemoveDir Enter clears the sub-mode"
    );
    // Esc path: 'd' then Esc cancels without shipping.
    app.dirs_cache = vec!["/tmp/extra".into()];
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Char('d')),
    );
    crate::keys::handle_working(&mut app, key_press(ratatui::crossterm::event::KeyCode::Esc));
    assert_eq!(
        app.permission_input,
        PermissionInput::None,
        "Esc cancels RemoveDir"
    );
    assert!(
        !app.dirs_cache.is_empty(),
        "Esc does not remove the dir locally (server is the authority)"
    );
}

#[test]
fn test_permission_add_bad_effect() {
    // A bad effect surfaces the parse error as a system line, not a crash,
    // and leaves the sub-mode.
    use crate::state::PermissionInput;
    let mut app = crate::composition::build_app_for_test(None);
    app.screen = crate::state::Screen::Working;
    app.pane = Pane::Permission;
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Char('a')),
    );
    for c in "bash bodge".chars() {
        crate::keys::handle_working(
            &mut app,
            key_press(ratatui::crossterm::event::KeyCode::Char(c)),
        );
    }
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Enter),
    );
    assert_eq!(app.permission_input, PermissionInput::None);
    let out = last_system(&app);
    assert!(out.contains("unknown effect"), "error surfaced: {out}");
}

#[test]
fn test_permission_palette_opens_pane() {
    // /permissions is a palette command (SlashCommand::Permission), and
    // running it opens the interactive pane.
    use houyicoder_protocol::frontend::SlashCommand;
    assert!(
        SlashCommand::ALL.contains(&SlashCommand::Permission),
        "Permission is in the palette"
    );
    assert_eq!(
        SlashCommand::parse("/permissions"),
        Some(SlashCommand::Permission)
    );
    assert_eq!(SlashCommand::Permission.name(), "/permissions");
    let mut app = app();
    app.run_command(SlashCommand::Permission);
    assert_eq!(app.pane, Pane::Permission, "run_command opens the pane");
}

#[test]
fn test_permissions_pane_renders_wired() {
    // In agent-chat mode (a wired session) the main area is the transcript
    // for most panes, but /permissions takes over the main area like the
    // artifact pane — so the tab header + rule list render, not the chat
    // stream. Guards the draw_main overlay routing.
    let mut app = crate::composition::build_app_for_test(None);
    app.screen = crate::state::Screen::Working;
    app.pane = Pane::Permission;
    let text = render_text(&app, 100, 28);
    assert!(
        text.contains("Allow"),
        "tab header renders in wired mode: {text}"
    );
    assert!(text.contains("Recent"), "Recent tab renders: {text}");
}
