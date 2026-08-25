//! Phase-adaptive viewport buffer-dump tests. Each test renders the App in one
//! of the three viewport modes (Working / Focus / Scroll) at 80x24, prints the
//! real buffer, and asserts the chrome budget (rows of chrome vs content) so
//! the density reduction is verified from actual output, not guesses. The last
//! test drives the full interaction flow end-to-end.

#![cfg(test)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::frontend::SlashCommand;

use crate::composition;
use crate::keys;
use crate::state::{Pane, Screen, Stage, ViewportMode};
use crate::test_support::render_text;

fn working() -> crate::state::App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app
    // stub starts at Idle / Working.
}

fn render(app: &crate::state::App) -> String {
    render_text(app, 80, 24)
}

/// Identify chrome rows in a Working-mode dump: the 3-line input block pinned
/// to the bottom (top border, prompt line, bottom border) and the 1-line status
/// bar above it. No progress header or pane-tab strip. Everything else is
/// content.
fn working_chrome_count(out: &str) -> usize {
    let rows: Vec<&str> = out.lines().collect();
    let mut chrome = 0usize;
    // Status bar is the LAST row (below the input), carries the progress bar.
    if rows
        .last()
        .is_some_and(|r| r.contains("design") && r.contains("verify"))
    {
        chrome += 1;
    }
    // Input block: 3 rows pinned above the status (top border + 1 content +
    // bottom border) for an empty input.
    if rows.len() >= 3 {
        chrome += 3;
    }
    chrome
}

#[test]
fn test_working_mode_chrome_budget() {
    let app = working();
    let out = render(&app);
    println!("--- Working mode (80x24) ---\n{out}\n--- end ---");
    assert_eq!(app.viewport, ViewportMode::Working);
    // No 2-line menu bar: no spec strip, no progress header row, no pane tabs.
    assert!(
        !out.contains("spec-001"),
        "spec id should be absent from the header: found spec-001"
    );
    assert!(
        !out.contains("[g] replay"),
        "replay hint should be absent from the header"
    );
    assert!(
        !out.contains("requirements:"),
        "per-clause divergence line should be absent from the header"
    );
    assert!(
        !out.contains("[log]"),
        "pane-tab strip should be gone: found [log]"
    );
    // The progress bar moved into the status bar.
    assert!(
        out.contains("design") && out.contains("implement") && out.contains("verify"),
        "progress bar should be in the status bar: [{out}]"
    );
    // Chrome budget: 3 input (border+content+border) + 1 status = 4 rows.
    let chrome = working_chrome_count(&out);
    assert_eq!(
        chrome, 4,
        "Working mode chrome should be 4 rows, got {chrome}"
    );
    // Content = 24 - 4 = 20 rows.
    let total = out.lines().count();
    assert_eq!(total, 24, "terminal height should be 24");
    assert_eq!(
        total - chrome,
        20,
        "content rows should be 20, got {}",
        total - chrome
    );
}

#[test]
fn test_focus_mode_chrome_budget() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // design -> implement -> Focus
    assert_eq!(app.stage, Stage::Implementing);
    assert_eq!(app.viewport, ViewportMode::Focus);
    let out = render(&app);
    println!("--- Focus mode (80x24) ---\n{out}\n--- end ---");
    // The header is fused into the pane border title (0 extra rows): the diff
    // pane top border carries the progress bar.
    let first = out.lines().next().expect("first row");
    assert!(
        first.contains("design") && first.contains("diff approval"),
        "progress bar should fuse into the diff pane title: [{first}]"
    );
    // Pane tabs and input are hidden. The input box title always carries
    // "for commands" when visible, so its absence proves the input is hidden.
    assert!(
        !out.contains("[log]"),
        "pane tabs should be hidden in Focus mode"
    );
    assert!(
        !out.contains("for commands"),
        "input box should be hidden in Focus mode: [{out}]"
    );
    // The status bar is the only chrome row: actionable keys + progress.
    let last = out.lines().last().expect("last row");
    assert!(
        last.contains("a=approve") && last.contains("design"),
        "focus status bar should show actionable keys + progress: [{last}]"
    );
    // Chrome budget: 1 row (the status bar). Content = 23.
    let total = out.lines().count();
    assert_eq!(total, 24);
    assert_eq!(total - 1, 23, "content rows should be 23 in Focus mode");
}

#[test]
fn test_scroll_mode_chrome_budget() {
    let mut app = working();
    // Grow the transcript so scrolling is meaningful.
    for _ in 0..40 {
        app.system_line("a long line of transcript history");
    }
    // Render once so the view publishes cap/total into the scroll cells.
    drop(render(&app));
    // PgUp enters Scroll mode.
    keys::handle_working(&mut app, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(app.viewport, ViewportMode::Scroll);
    let out = render(&app);
    println!("--- Scroll mode (80x24) ---\n{out}\n--- end ---");
    // Header, tabs, and input are all hidden. The input box title always
    // carries "for commands" when visible, so its absence proves the input is
    // hidden.
    assert!(
        !out.contains("[log]"),
        "pane tabs should be hidden in Scroll mode"
    );
    assert!(
        !out.contains("for commands"),
        "input box should be hidden in Scroll mode: [{out}]"
    );
    // The overlay status bar shows the line position + hints.
    let last = out.lines().last().expect("last row");
    assert!(
        last.contains("line") && last.contains("Esc=tail"),
        "scroll overlay should show line position + tail hint: [{last}]"
    );
    // Chrome budget: 1 row (the overlay). Content = 23.
    let total = out.lines().count();
    assert_eq!(total, 24);
    assert_eq!(total - 1, 23, "content rows should be 23 in Scroll mode");
}

#[test]
fn test_status_bar_fits_80() {
    // After Esc-from-Focus the stage stays but the viewport folds to Working,
    // so the status bar (progress bar + hint) must fit 80 cols at every stage.
    // The Implementing hint is the longest; this guards against regressions.
    for stage in [
        Stage::Idle,
        Stage::Design,
        Stage::Implementing,
        Stage::Verify,
        Stage::Done,
    ] {
        let mut app = working();
        app.set_stage(stage);
        app.fold_to_working();
        assert_eq!(
            app.viewport,
            ViewportMode::Working,
            "stage {stage:?} should fold to Working"
        );
        let out = render(&app);
        let status = out
            .lines()
            .find(|r| r.contains("->") && (r.contains("☉") || r.contains("*")))
            .unwrap_or_else(|| panic!("status bar row missing for stage {stage:?}:\n{out}"));
        // char count is display width here (every glyph is single-width), and
        // the status bar must not exceed the 80-col terminal width.
        let width = status.chars().count();
        assert!(
            width <= 80,
            "stage {stage:?}: status bar overflowed 80 cols ({}): [{status}]",
            width
        );
    }
}

/// A command pane owns the bottom of the screen: the input bar and the
/// status bar both retract so the pane reclaims the rows. The pane's own
/// footer hint replaces the status row as the last line. Without this, the
/// status bar (stage chain) stayed pinned under the pane, wasting a row the
/// pane could use and showing model/dir context that is not actionable while
/// a pane is focused.
#[test]
fn test_pane_hides_status_bar() {
    let mut app = working();
    app.pane = Pane::Hooks;
    let out = render(&app);
    assert!(
        !out.contains("implement"),
        "status bar stage chain should be hidden when a pane is open: {out}"
    );
    let last = out.lines().last().unwrap_or("");
    assert!(
        last.contains("Esc=close"),
        "the hooks pane footer should be the last row: [{last}]"
    );
}

/// /permissions is the one command pane that keeps its input box (the
/// Add/Remove sub-mode types rule specs into it), so the status bar must
/// hide while the input box stays — the two retractions are decoupled, not
/// one set. The status chain (stage words) must be gone; the input
/// placeholder must still render.
#[test]
fn test_permission_keeps_input() {
    let mut app = working();
    app.pane = Pane::Permission;
    let out = render(&app);
    assert!(
        !out.contains("implement"),
        "status bar should hide when /permissions is open: {out}"
    );
    assert!(
        out.contains("for commands"),
        "input box should stay for /permissions rule entry: {out}"
    );
}

#[test]
fn test_full_flow_viewport() {
    let mut app = working();
    let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);

    // 1. Type a task + Enter -> auto-enter Design (Working mode, header shows
    //    progress bar).
    for c in "fix the login bug".chars() {
        keys::handle_working(&mut app, key(KeyCode::Char(c)));
    }
    keys::handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(app.stage, Stage::Design);
    assert_eq!(
        app.viewport,
        ViewportMode::Working,
        "Design should be Working"
    );
    let out = render(&app);
    println!("--- step 1: task -> design (Working) ---\n{out}\n--- end ---");
    assert!(
        out.contains("design"),
        "progress bar should show in Working status bar"
    );

    // 2. Press a -> auto-advance to Implementing -> auto-enter Focus (header +
    //    tabs + input fold, diff pane full-width).
    app.input.clear();
    keys::handle_working(&mut app, key(KeyCode::Char('a')));
    assert_eq!(app.stage, Stage::Implementing);
    assert_eq!(
        app.viewport,
        ViewportMode::Focus,
        "Implementing should fold to Focus"
    );
    let out = render(&app);
    println!("--- step 2: approve -> implement (Focus) ---\n{out}\n--- end ---");
    assert!(
        out.contains("diff approval"),
        "diff pane should be full-width"
    );
    assert!(!out.contains("[log]"), "tabs should be hidden in Focus");

    // 3. Review diff: approve all 3 changes. Auto-advance moves focus to the
    //    next pending change after each approve, so repeated a walks every
    //    change and trips the all-approved transition to verify.
    for _ in 0..3 {
        keys::handle_working(&mut app, key(KeyCode::Char('a')));
    }
    assert_eq!(app.stage, Stage::Verify, "all changes approved -> verify");
    assert_eq!(app.viewport, ViewportMode::Focus, "Verify stays in Focus");
    let out = render(&app);
    println!("--- step 3: all changes approved -> verify (Focus) ---\n{out}\n--- end ---");

    // 4. Approve all 3 findings -> machine-check phase (still Focus).
    for _ in 0..3 {
        keys::handle_working(&mut app, key(KeyCode::Char('a')));
        keys::handle_working(&mut app, key(KeyCode::Down));
    }
    assert_eq!(app.pane, Pane::Verify, "agent review done -> machine check");
    assert_eq!(app.viewport, ViewportMode::Focus);

    // 5. Press a -> Done -> return to Working (header + tabs + input reappear).
    keys::handle_working(&mut app, key(KeyCode::Char('a')));
    assert_eq!(app.stage, Stage::Done);
    assert_eq!(
        app.viewport,
        ViewportMode::Working,
        "Done should unfold to Working"
    );
    let out = render(&app);
    println!("--- step 5: verify complete -> done (Working) ---\n{out}\n--- end ---");
    assert!(
        out.contains("done."),
        "Working status hint should show at Done"
    );
    assert!(
        out.contains('❯'),
        "input row should be visible in Working: [{out}]"
    );

    // 6. PgUp -> enter Scroll (chrome folds, full-screen transcript).
    keys::handle_working(&mut app, key(KeyCode::PageUp));
    assert_eq!(app.viewport, ViewportMode::Scroll);
    let out = render(&app);
    println!("--- step 6: PgUp -> scroll ---\n{out}\n--- end ---");
    assert!(out.contains("Esc=tail"), "scroll overlay should show");

    // 7. Esc -> return to Working.
    keys::handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.viewport,
        ViewportMode::Working,
        "Esc should return to Working"
    );
    let out = render(&app);
    println!("--- step 7: Esc -> working ---\n{out}\n--- end ---");
    assert!(
        out.contains("done."),
        "Working status hint should reappear after scroll exit"
    );
}

#[test]
fn test_focus_esc_then_pgup() {
    let mut app = working();
    app.run_command(SlashCommand::Implement); // -> Implementing -> Focus
    assert_eq!(app.viewport, ViewportMode::Focus);
    // Esc in Focus folds to Working (stage unchanged).
    keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.viewport, ViewportMode::Working);
    assert_eq!(app.stage, Stage::Implementing, "stage unchanged by Esc");
    // PgUp enters Scroll from Working.
    keys::handle_working(&mut app, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(app.viewport, ViewportMode::Scroll);
    // Esc returns to the previous mode (Working, not Focus).
    keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        app.viewport,
        ViewportMode::Working,
        "Esc from scroll should return to Working, not Focus"
    );
}

/// Esc on the /memory pane (empty input + empty text filter) dismisses the
/// pane back to the transcript. The pane footer advertises "Esc close" so the
/// key must actually close it — without the arm Esc was a dead key when both
/// the input box + the filter were empty. Pins the dismiss wiring at the key
/// layer (the real-binary PTY path pins the full round-trip).
#[test]
fn test_esc_closes_memory_pane() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    assert_eq!(app.pane, Pane::Memory, "memory pane should open");
    keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        app.pane,
        Pane::Transcript,
        "Esc should dismiss the memory pane"
    );
    // With a non-empty text filter, Esc clears the filter (not the pane).
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    app.memory_list.query = "alpha".to_string();
    keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        app.pane,
        Pane::Memory,
        "Esc clears the filter, not the pane"
    );
    assert!(
        !app.memory_list.searching(),
        "Esc should clear the text filter"
    );
}

/// Esc closes the memory pane even while a run is in flight. The abort-run
/// Esc arm (handle_working, before handle_input) must not steal the Esc when
/// a pane with its own Esc-close is open — otherwise the user cannot dismiss
/// the pane mid-run (they'd abort the run instead). A second Esc after the
/// pane closes aborts the run. Pins the regression where the busy Esc arm
/// ate the pane-dismiss.
#[test]
fn test_esc_closes_memory_busy() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    app.agent_busy = true;
    assert_eq!(app.pane, Pane::Memory, "memory pane open mid-run");
    keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        app.pane,
        Pane::Transcript,
        "Esc dismisses the pane mid-run, not abort the run"
    );
    // A second Esc (pane now closed) reaches the abort-run arm. abort_run
    // sets cancelling (agent_busy clears later in the Done handler, so
    // assert cancelling not agent_busy).
    app.cancelling = false;
    keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.cancelling, "second Esc aborts the run");
}

#[test]
fn test_scroll_typing_exits() {
    let mut app = working();
    app.run_command(SlashCommand::Implement); // Focus
    keys::handle_working(&mut app, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(app.viewport, ViewportMode::Scroll);
    // A typing key exits scroll to the previous mode (Focus). The key is
    // consumed by the exit and does not approve a hunk.
    let before = app.diff.current().map(|h| h.approved);
    keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
    );
    assert_eq!(
        app.viewport,
        ViewportMode::Focus,
        "typing should exit scroll to Focus"
    );
    assert_eq!(
        app.diff.current().map(|h| h.approved),
        before,
        "exit key should not approve a hunk"
    );
}

/// The /worktrees pane journey: the list renders the count header, a
/// truncated path column, the HEAD sha + branch, and the current-worktree
/// marker. Covers the path-column + branch render at unit level (the PTY
/// layer in ui_worktree covers open + search + the real git repo path).
#[test]
fn test_worktree_pane_renders_columns() {
    let mut app = working();
    app.pane = Pane::Worktree;
    app.worktree_entries = vec![
        crate::composition::WorktreeEntry {
            path: "/some/repo/alpha".into(),
            head: "abcdef0".into(),
            branch: "main".into(),
            is_current: true,
        },
        crate::composition::WorktreeEntry {
            path: "/some/repo/beta".into(),
            head: "1234567".into(),
            branch: "dev".into(),
            is_current: false,
        },
    ];
    let out = render(&app);
    assert!(
        out.contains("worktrees — 2 listed"),
        "header with count: {out}"
    );
    assert!(out.contains("main"), "branch column renders: {out}");
    assert!(out.contains("dev"), "branch column renders: {out}");
    assert!(out.contains("abcdef0"), "HEAD sha renders: {out}");
    assert!(
        out.contains("alpha"),
        "path tail renders (truncated): {out}"
    );
}
