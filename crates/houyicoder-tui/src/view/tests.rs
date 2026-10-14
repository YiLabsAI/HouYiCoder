//! The view module's own tests: badge colours and the rendered status bar,
//! plus the buffer-dump invariant suite and the pane/drag selection suites.

use crate::composition;
use crate::state::Screen;
use crate::test_harness::render_text;

use super::badge_color;
use ratatui::style::Color;

#[test]
fn test_badge_color_known() {
    assert_eq!(badge_color("red"), Some(Color::Red));
    assert_eq!(badge_color("green"), Some(Color::Green));
    assert_eq!(badge_color("blue"), Some(Color::Blue));
    assert_eq!(badge_color("yellow"), Some(Color::Yellow));
    assert_eq!(badge_color("cyan"), Some(Color::Cyan));
    assert_eq!(badge_color("magenta"), Some(Color::Magenta));
}

#[test]
fn test_badge_color_unknown() {
    assert_eq!(badge_color("chartreuse"), None);
    assert_eq!(badge_color(""), None);
}

#[test]
fn test_dump_working_after_login() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    let out = render_text(&app, 80, 24);
    // sanity: status bar tiny mark present
    assert!(
        out.contains("☉") || out.contains("*"),
        "tiny mark missing:\n{out}"
    );
    // landing surface is clean: the input box carries the placeholder
    // hint, no welcome line floating above the input.
    assert!(
        out.contains("let's build"),
        "input placeholder missing from working surface:\n{out}"
    );
    println!("--- working screen after login (80x24) ---\n{out}\n--- end ---");
}

#[test]
fn test_dump_palette_render() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.open_palette();
    let out = render_text(&app, 80, 24);
    println!("--- palette AFTER (80x24) ---\n{out}\n--- end ---");
    let rows: Vec<&str> = out.lines().collect();

    // Invariant: the popover title row keeps its Esc hint intact and every
    // list row stays inside the inner area, so the first list row must not
    // overlap the title's Esc hint and must end at the right border before
    // the popover edge.

    // 1. Find the popover title row (contains "/ commands" + "filter:").
    let title_row = rows
        .iter()
        .find(|r| r.contains("/ commands") && r.contains("filter:"))
        .copied()
        .expect("popover title row present");
    assert!(
        title_row.contains("Esc to close"),
        "title must keep Esc to close intact (no overlap), got [{title_row}]"
    );

    // 2. The first list row shows /login and its help must not run past
    //    the right border, and the help must not contain "Esc".
    let login_row = rows
        .iter()
        .find(|r| r.contains("/login"))
        .copied()
        .expect("first list row present");
    assert!(
        !login_row.contains("Esc"),
        "first list row must not overlap the title's Esc hint: [{login_row}]"
    );
    assert!(
        login_row.contains("│"),
        "first list row must end at the right border: [{login_row}]"
    );

    // 3. A long help is truncated with an ellipsis and stays inside the
    //    border (no overflow into the border column).
    let truncated_row = rows
        .iter()
        .find(|r| r.contains("/clear"))
        .copied()
        .expect("/clear row present");
    assert!(
        truncated_row.contains('\u{2026}'),
        "long /clear help should be truncated with ellipsis: [{truncated_row}]"
    );
    assert!(
        truncated_row.contains("│"),
        "/clear row must end at the right border: [{truncated_row}]"
    );
}

#[test]
fn test_status_bar_is_contextual() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    let out = render_text(&app, 80, 24);
    println!("--- status bar (80x24) ---\n{out}\n--- end ---");
    // The status bar sits above the bottom-pinned input box; find it by its
    // tiny logo mark rather than by screen position.
    let status = out
        .lines()
        .find(|r| r.contains("->") && (r.contains("☉") || r.contains("*")))
        .expect("status bar row with tiny mark")
        .to_string();
    // The status bar carries a contextual stage hint, not a static
    // model/tokens/sandbox banner. The glyph token display (arrows +
    // cache pct) is also banned: noise the /context view already carries.
    for dropped in [
        "model:", "tokens:", "sandbox:", "session:", "cap:", "↑", "↓", "cache ",
    ] {
        assert!(
            !status.contains(dropped),
            "dropped field {dropped} still in status bar: [{status}]"
        );
    }
    // Tiny mark prefix and the progress bar are present.
    assert!(
        status.contains("->") && (status.contains("☉") || status.contains("*")),
        "tiny mark prefix missing: [{status}]"
    );
    assert!(
        status.contains("design") && status.contains("verify"),
        "progress bar missing from status bar: [{status}]"
    );
    // The whole status bar fits in 80 cols.
    assert!(
        status.len() <= 80,
        "status bar must fit one line at 80 cols, got {}: [{status}]",
        status.len()
    );
}

mod invariants;
