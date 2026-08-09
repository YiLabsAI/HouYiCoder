//! Render-focused tests for the /permissions pane: tab filtering, the
//! canonical rule label, the Recently-denied body, and the live SearchBox
//! filter. Split from permission_tests.rs so neither file breaches the
//! file-size gate. Drives the real App render path (no runner needed — the
//! pane is a TUI-local surface that renders in stub mode).

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

fn key_press(code: ratatui::crossterm::event::KeyCode) -> ratatui::crossterm::event::KeyEvent {
    ratatui::crossterm::event::KeyEvent {
        code,
        modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
        kind: ratatui::crossterm::event::KeyEventKind::Press,
        state: ratatui::crossterm::event::KeyEventState::NONE,
    }
}

#[test]
fn test_permission_tab_filters_rules() {
    // Each rule tab shows only its own effect's rules; Recently-denied shows
    // denials. Labels render in the canonical form (e.g. "Bash(npm:*)").
    use houyicoder_protocol::frontend::permission::{
        PermissionEffect, PermissionRule, PermissionRuleContent,
    };
    let rule = |action: &str, effect: PermissionEffect, content: Option<&str>| PermissionRule {
        action: action.to_string(),
        content: content.map(|v| PermissionRuleContent::Prefix {
            value: v.to_string(),
        }),
        effect,
        ..Default::default()
    };
    let mut app = app();
    app.pane = Pane::Permission;
    app.rules_cache = vec![
        rule("bash", PermissionEffect::Allow, Some("npm")),
        rule("bash", PermissionEffect::Reject, Some("rm")),
        rule("edit", PermissionEffect::Ask, None),
    ];
    // Allow tab: npm rule only.
    app.permission_tab = crate::state::PermissionTab::Allow;
    let text = render_text(&app, 80, 24);
    assert!(
        text.contains("Bash(npm"),
        "Allow tab shows the npm rule: {text}"
    );
    assert!(
        !text.contains("Bash(rm"),
        "Allow tab hides the deny rule: {text}"
    );
    // Deny tab: rm rule only.
    app.permission_tab = crate::state::PermissionTab::Deny;
    let text = render_text(&app, 80, 24);
    assert!(
        text.contains("Bash(rm"),
        "Deny tab shows the rm rule: {text}"
    );
    assert!(
        !text.contains("Bash(npm"),
        "Deny tab hides the allow rule: {text}"
    );
    // Ask tab: tool-level rule renders as "Edit" (no content).
    app.permission_tab = crate::state::PermissionTab::Ask;
    let text = render_text(&app, 80, 24);
    assert!(text.contains("Edit"), "Ask tab shows the edit rule: {text}");
    assert!(
        !text.contains("Bash(npm"),
        "Ask tab hides the allow rule: {text}"
    );
}

#[test]
fn test_permission_cursor_recent_tab() {
    // Up/Down move the cursor; the Recently-denied tab renders the denial log;
    // Exact + Glob content arms render.
    use houyicoder_protocol::frontend::permission::{
        PermissionDecisionEntry, PermissionEffect, PermissionRule, PermissionRuleContent,
    };
    let mut app = app();
    app.pane = Pane::Permission;
    app.rules_cache = vec![
        PermissionRule {
            action: "bash".into(),
            content: Some(PermissionRuleContent::Exact {
                value: "git push".into(),
            }),
            effect: PermissionEffect::Allow,
            ..Default::default()
        },
        PermissionRule {
            action: "bash".into(),
            content: Some(PermissionRuleContent::Glob {
                value: "git *".into(),
            }),
            effect: PermissionEffect::Allow,
            ..Default::default()
        },
    ];
    let key = |code: ratatui::crossterm::event::KeyCode| ratatui::crossterm::event::KeyEvent {
        code,
        modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
        kind: ratatui::crossterm::event::KeyEventKind::Press,
        state: ratatui::crossterm::event::KeyEventState::NONE,
    };
    // Down moves the cursor to row 1; Up moves it back to 0 (saturating).
    crate::keys::handle_working(&mut app, key(ratatui::crossterm::event::KeyCode::Down));
    assert_eq!(app.permission_cursor, 1);
    crate::keys::handle_working(&mut app, key(ratatui::crossterm::event::KeyCode::Down));
    assert_eq!(
        app.permission_cursor, 2,
        "cursor keeps growing; render clamps"
    );
    crate::keys::handle_working(&mut app, key(ratatui::crossterm::event::KeyCode::Up));
    assert_eq!(app.permission_cursor, 1);
    crate::keys::handle_working(&mut app, key(ratatui::crossterm::event::KeyCode::Up));
    assert_eq!(app.permission_cursor, 0);
    crate::keys::handle_working(&mut app, key(ratatui::crossterm::event::KeyCode::Up));
    assert_eq!(app.permission_cursor, 0, "saturating sub stops at 0");

    // Allow tab renders Exact and Glob arms as "Bash(git push)" and "Bash(git *)".
    let text = render_text(&app, 80, 24);
    assert!(
        text.contains("Bash(git push)"),
        "Exact content rendered: {text}"
    );
    assert!(
        text.contains("Bash(git *)"),
        "Glob content rendered: {text}"
    );

    // Recently-denied tab: empty verdicts show the placeholder.
    app.permission_tab = crate::state::PermissionTab::Recent;
    let text = render_text(&app, 80, 24);
    assert!(
        text.contains("no recent denials"),
        "empty Recently-denied tab: {text}"
    );

    // Recently-denied with a denial shows the row (✗ tool + scope); allows
    // never surface here.
    app.verdict_log_cache = vec![PermissionDecisionEntry {
        tool: "bash".into(),
        verdict: "deny".into(),
        scope: "rm -rf".into(),
        call_id: "call_9".into(),
    }];
    let text = render_text(&app, 80, 24);
    assert!(
        text.contains("bash"),
        "Recently-denied shows the tool: {text}"
    );
    assert!(
        text.contains("rm -rf"),
        "Recently-denied shows the scope: {text}"
    );
}

#[test]
fn test_permission_search_submode_filters() {
    // 's' enters Search; typing filters the current tab live (the query lives
    // in the re-purposed input box, mirrored by the SearchBox); Enter ends
    // search; Esc clears the query.
    use crate::state::{PermissionInput, PermissionTab};
    use houyicoder_protocol::frontend::permission::{
        PermissionEffect, PermissionRule, PermissionRuleContent,
    };
    let mut app = app();
    app.pane = Pane::Permission;
    app.rules_cache = vec![
        PermissionRule {
            action: "bash".into(),
            content: Some(PermissionRuleContent::Prefix {
                value: "npm".into(),
            }),
            effect: PermissionEffect::Allow,
            ..Default::default()
        },
        PermissionRule {
            action: "edit".into(),
            content: Some(PermissionRuleContent::Prefix {
                value: "src".into(),
            }),
            effect: PermissionEffect::Allow,
            ..Default::default()
        },
    ];
    app.permission_tab = PermissionTab::Allow;
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Char('s')),
    );
    assert_eq!(app.permission_input, PermissionInput::Search);
    for c in "npm".chars() {
        crate::keys::handle_working(
            &mut app,
            key_press(ratatui::crossterm::event::KeyCode::Char(c)),
        );
    }
    let text = render_text(&app, 100, 28);
    assert!(
        text.contains("Bash(npm"),
        "search keeps the matching rule: {text}"
    );
    assert!(
        !text.contains("src"),
        "search hides the non-matching rule: {text}"
    );
    // Enter ends search; Esc after that clears the query and exits search.
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Enter),
    );
    assert_eq!(app.permission_input, PermissionInput::None);
}

#[test]
fn test_pane_renders_pane_frame() {
    // The /permissions surface uses the Pane primitive: a full-width ─ Divider
    // framing the region (not a rounded border, not a full-screen overlay), and
    // a "Permissions:" prefix on the tab row. Both modes (stub + wired) render
    // inline below the transcript tail — no stub-vs-wired style divergence.
    let mut app = app();
    app.pane = Pane::Permission;
    let text = render_text(&app, 100, 28);
    assert!(
        text.contains("Permissions:"),
        "Pane content carries the Permissions: prefix: {text}"
    );
    assert!(
        text.contains('\u{2500}'),
        "Pane draws the ─ Divider line: {text}"
    );
}

#[test]
fn test_permission_workspace_cursor_clamps() {
    // Regression: the Workspace cursor used to clamp against max(rules,
    // denials), so a denial log longer than the dirs list let the cursor run
    // past the dirs (the highlight vanished). It must clamp against the dirs
    // list on the Workspace tab. Asserted via cell style: the cyan+bold row
    // lands on a dir, not beyond.
    use crate::state::PermissionTab;
    use ratatui::style::{Color, Modifier, Style};
    let mut app = app();
    app.pane = Pane::Permission;
    app.permission_tab = PermissionTab::Workspace;
    app.working_dir = "/repo".into();
    app.dirs_cache = vec!["/tmp/a".into(), "/tmp/b".into()];
    // A denial log longer than the dirs list — the trap condition.
    use houyicoder_protocol::frontend::permission::PermissionDecisionEntry;
    app.verdict_log_cache = (0..5)
        .map(|i| PermissionDecisionEntry {
            tool: "bash".into(),
            verdict: "deny".into(),
            scope: format!("rm-{i}"),
            call_id: format!("c{i}"),
        })
        .collect();
    // Cursor set past the dirs list — must clamp to the last dir (index 1).
    app.permission_cursor = 4;
    let buf = crate::test_support::render_buffer(&app, 80, 24);
    // Partial cell-style match: ratatui composes bg(Reset)/underline_color(Reset)
    // into the stored style, so a full Style == comparison never matches.
    let is_cyan_bold =
        |st: &Style| st.fg == Some(Color::Cyan) && st.add_modifier.contains(Modifier::BOLD);
    // Find the cyan+bold row that is a DIR (contains /tmp/), skipping the
    // Divider + tab header (also cyan+bold). The clamped cursor must land on
    // a dir row, not vanish past the list.
    let mut highlighted_dir: Option<String> = None;
    for y in 0..buf.area.height {
        let row_text: String = (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>();
        if !row_text.contains("/tmp/") {
            continue;
        }
        let has_cursor = (0..buf.area.width).any(|x| is_cyan_bold(&buf[(x, y)].style()));
        if has_cursor {
            highlighted_dir = Some(row_text);
            break;
        }
    }
    let row = highlighted_dir.expect("a dir row must be highlighted (cyan+bold)");
    assert!(
        row.contains("/tmp/b"),
        "cursor clamps to the last dir (index 1): {row:?}"
    );
}

#[test]
fn test_permission_workspace_lists_dirs() {
    // The Workspace tab shows the original cwd (non-deletable) plus one row
    // per runtime-added directory; the empty state names what the tab holds.
    use crate::state::PermissionTab;
    let mut app = app();
    app.pane = Pane::Permission;
    app.permission_tab = PermissionTab::Workspace;
    app.working_dir = "/repo".into();
    app.dirs_cache = vec!["/tmp/extra".into()];
    let text = render_text(&app, 80, 24);
    assert!(
        text.contains("original working directory"),
        "Workspace shows the original cwd: {text}"
    );
    assert!(
        text.contains("/tmp/extra"),
        "Workspace shows an added directory: {text}"
    );
    // Empty state.
    app.dirs_cache.clear();
    let text = render_text(&app, 80, 24);
    assert!(
        text.contains("no additional directories"),
        "Workspace empty state: {text}"
    );
}

#[test]
fn test_workspace_nav_two_dirs() {
    // Up/Down moves the cursor highlight across multiple dirs (the user only
    // tested with one dir, so the multi-dir case was uncovered). With two
    // dirs, cursor 0 highlights dir A; cursor 1 highlights dir B. Asserted
    // via a partial cell-style match (fg == Cyan + BOLD) — ratatui composes
    // bg(Reset)/underline_color(Reset) into the stored style, so a full
    // Style == comparison would never match.
    use crate::state::PermissionTab;
    use ratatui::style::{Color, Modifier, Style};
    let is_cyan_bold =
        |st: &Style| st.fg == Some(Color::Cyan) && st.add_modifier.contains(Modifier::BOLD);
    let mut app = app();
    app.pane = Pane::Permission;
    app.permission_tab = PermissionTab::Workspace;
    app.working_dir = "/repo".into();
    app.dirs_cache = vec!["/tmp/a".into(), "/tmp/b".into()];

    let scan = |app: &App| -> (bool, bool) {
        let buf = crate::test_support::render_buffer(app, 80, 24);
        let mut a = false;
        let mut b = false;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>();
            let has_cursor = (0..buf.area.width).any(|x| is_cyan_bold(&buf[(x, y)].style()));
            if has_cursor && row.contains("/tmp/a") {
                a = true;
            }
            if has_cursor && row.contains("/tmp/b") {
                b = true;
            }
        }
        (a, b)
    };

    app.permission_cursor = 0;
    let (a, b) = scan(&app);
    assert!(a, "cursor 0 highlights dir A");
    assert!(!b, "cursor 0 does not highlight dir B");

    app.permission_cursor = 1;
    let (a, b) = scan(&app);
    assert!(b, "cursor 1 highlights dir B");
    assert!(!a, "cursor 1 does not highlight dir A");
}

#[test]
fn test_destination_arrows_match_direction() {
    // Regression: Left/Right in the destination picker used to move the
    // OPPOSITE visual direction. Visual order is [project user local]; Right
    // moves the focus right (project->user->local), Left moves it left
    // (project->local->user, wrapping). Asserted on the state (the
    // AddDestination.destination field after each arrow), not the render —
    // ratatui's incremental diff does not always re-emit the changed bracket
    // bytes, so a render-buffer assertion would be a false negative.
    use crate::state::PermissionInput;
    use houyicoder_protocol::frontend::permission::{PermissionEffect, RuleDestination};
    let mut app = app();
    app.pane = Pane::Permission;
    app.permission_input = PermissionInput::AddDestination {
        action: "bash".into(),
        content: None,
        effect: PermissionEffect::Allow,
        destination: RuleDestination::Project,
    };
    let dest = |app: &App| match &app.permission_input {
        PermissionInput::AddDestination { destination, .. } => *destination,
        _ => unreachable!(),
    };
    // Right moves visually right: project -> user -> local -> (wrap) project.
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Right),
    );
    assert_eq!(dest(&app), RuleDestination::User, "Right: project -> user");
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Right),
    );
    assert_eq!(dest(&app), RuleDestination::Local, "Right: user -> local");
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Right),
    );
    assert_eq!(
        dest(&app),
        RuleDestination::Project,
        "Right: local -> project (wrap)"
    );
    // Left moves visually left: project -> local -> user -> (wrap) project.
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Left),
    );
    assert_eq!(dest(&app), RuleDestination::Local, "Left: project -> local");
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Left),
    );
    assert_eq!(dest(&app), RuleDestination::User, "Left: local -> user");
    crate::keys::handle_working(
        &mut app,
        key_press(ratatui::crossterm::event::KeyCode::Left),
    );
    assert_eq!(
        dest(&app),
        RuleDestination::Project,
        "Left: user -> project (wrap)"
    );
}
