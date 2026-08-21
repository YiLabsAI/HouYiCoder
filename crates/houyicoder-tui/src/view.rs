//! View module: pure rendering of App state. No mutation, no I/O. Each screen
//! and overlay has its own submodule so files stay small.

pub mod approval;
pub mod artifact;
pub mod ask_question;
pub mod capability;
pub mod components;
pub mod console;
pub mod context_view;
#[cfg(test)]
pub mod drag_copy_tests;
#[cfg(test)]
pub mod drag_select_bug_tests;
pub mod export_log;
pub mod hooks_pane;
pub mod input_bar;
pub mod login;
pub mod logo;
pub mod markers;
pub mod memory_pane;
pub mod model_pane;
pub mod palette;
pub mod pane;
#[cfg(test)]
pub mod pane_select_tests;
pub mod queue_overlay;
pub mod resume_picker;
pub mod spinner;
pub mod status;
pub mod todo_list;
pub mod trajectory_pane;
pub mod word_diff;
pub mod working;
pub mod worktree_pane;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
};

use crate::state::{App, Screen};

/// Top-level draw: pick a screen. The slash palette is rendered inline as
/// part of the working-surface layout (see working::draw), not as a floating
/// overlay, so opening it pushes the transcript up rather than covering it.
/// The approval card is likewise inline at the transcript tail (see
/// working::draw_transcript), not a floating popup.
pub fn draw(f: &mut Frame, app: &App) {
    // Zero the jump-to-bottom pill rect at the top of every frame so it can
    // never go stale across screens that don't render a transcript (Login,
    // Console) — draw_transcript re-publishes it when the pill is visible.
    app.jump_pill_rect.set(Rect::new(0, 0, 0, 0));
    match app.screen {
        Screen::Login => login::draw(f, app),
        Screen::Console => console::draw(f, app),
        Screen::Working => working::draw(f, app),
    }
    // The /search slash-command result renders inline as a pane inside the
    // working surface (see working::draw_search_pane), not as a centered
    // popup overlay, so opening it does not cover the transcript.
}

/// A centered rect of the given percent width/height within area.
pub fn centered(width_pct: u16, height_pct: u16, area: Rect) -> Rect {
    let pop = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_pct) / 2),
            Constraint::Percentage(height_pct),
            Constraint::Percentage((100 - height_pct) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(pop[1])[1]
}

#[cfg(test)]
mod tests {
    use crate::composition;
    use crate::test_support::render_text;

    #[test]
    fn test_dump_working_after_login() {
        let mut app = composition::app();
        app.screen = crate::state::Screen::Working;
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
        app.screen = crate::state::Screen::Working;
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
            title_row.contains("Esc=close"),
            "title must keep Esc=close intact (no overlap), got [{title_row}]"
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
        app.screen = crate::state::Screen::Working;
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
        // model/tokens/sandbox banner.
        for dropped in ["model:", "tokens:", "sandbox:", "session:", "cap:"] {
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
}
pub mod line_wrap;
