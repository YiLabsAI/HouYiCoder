//! Tests for what the worktree pane draws at each level.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::worktree::WorktreePreviewView;

use super::*;
use crate::composition;
use crate::composition::WorktreeEntry;
use crate::state::App;
use crate::state::{Pane, Screen};
use crate::test_harness::render_buffer;
use crate::test_harness::render_text;
use crate::test_harness::worktree_path;
use crate::test_harness::worktree_preview;
use ratatui::buffer::Buffer;

fn pane_app() -> App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Worktree;
    app.sessions.current.capabilities.worktree.entries = vec![WorktreeEntry {
        path: "/some/repo/alpha".into(),
        head: "abcdef0".into(),
        branch: "main".into(),
        ..Default::default()
    }];
    app
}

fn with_preview(preview: WorktreePreviewView, level: WorktreeLevel) -> App {
    let app = pane_app();
    {
        let mut draft = app.ui.worktree_pane.draft.borrow_mut();
        draft.begin(&worktree_path("alpha"), WorktreeLevel::List, None);
        draft.accept(preview);
    }
    app.ui.worktree_pane.level.set(level);
    app
}

fn render(app: &App) -> String {
    render_text(app, 80, 24)
}

/// The detail fills its measured rows from the preview rather than leaving the
/// listing's values, and names the tree as free when nothing holds it.
#[test]
fn test_detail_shows_preview() {
    let app = with_preview(worktree_preview("alpha"), WorktreeLevel::Detail);
    let out = render(&app);
    assert!(
        out.contains("Merge:    unmerged: 2 commits ahead of dev"),
        "{out}"
    );
    assert!(
        out.contains("Changes:  2 tracked dirty · 1 untracked"),
        "{out}"
    );
    assert!(
        out.contains("Ignored:  target (build output) · 3 other"),
        "{out}"
    );
    assert!(out.contains("Use:      free"), "{out}");
}

/// A detail with nothing selected says so instead of drawing a blank block.
#[test]
fn test_detail_names_no_selection() {
    let mut app = pane_app();
    app.sessions.current.capabilities.worktree.entries.clear();
    app.ui.worktree_pane.level.set(WorktreeLevel::Detail);
    let out = render(&app);
    assert!(out.contains("(no worktree selected)"), "{out}");
}

/// A tree a session holds, or Git records as locked, is named on the
/// confirmation so the button is not the only thing standing in the way.
#[test]
fn test_confirm_shows_blocked() {
    let mut leased = worktree_preview("alpha");
    leased.leased = true;
    let out = render(&with_preview(leased, WorktreeLevel::Confirm));
    assert!(out.contains("Blocked:  in use by a session"), "{out}");

    let mut locked = worktree_preview("alpha");
    locked.locked = true;
    let out = render(&with_preview(locked, WorktreeLevel::Confirm));
    assert!(out.contains("Blocked:  locked in Git"), "{out}");
}

/// The branch choice reads as what it does in each position, so b is never a
/// guess about which way it toggles.
#[test]
fn test_confirm_drops_branch() {
    let app = with_preview(worktree_preview("alpha"), WorktreeLevel::Confirm);
    app.ui
        .worktree_pane
        .draft
        .borrow_mut()
        .toggle_branch_deletion();
    let out = render(&app);
    assert!(
        out.contains("deleted with the worktree, unmerged commits included (b to keep)"),
        "{out}"
    );
}

/// A merged branch goes with the tree whatever the toggle says, because its
/// commits are already in the base, so the row stops promising to keep it.
#[test]
fn test_merged_branch_deleted() {
    let mut preview = worktree_preview("alpha");
    preview.merged = true;
    let app = with_preview(preview, WorktreeLevel::Confirm);
    app.ui
        .worktree_pane
        .draft
        .borrow_mut()
        .toggle_branch_deletion();
    let out = render(&app);
    assert!(
        out.contains("deleted with the worktree, already merged into dev"),
        "{out}"
    );
    assert!(!out.contains("(b to keep)"), "{out}");
}

/// A 20-row terminal still shows the keys that act on the detail and on the
/// confirmation, instead of clipping them off the bottom.
#[test]
fn test_small_terminal_keeps_hint() {
    let detail = with_preview(worktree_preview("alpha"), WorktreeLevel::Detail);
    let out = render_text(&detail, 80, 20);
    assert!(out.contains("d to remove"), "{out}");

    let confirm = with_preview(worktree_preview("alpha"), WorktreeLevel::Confirm);
    let out = render_text(&confirm, 80, 20);
    assert!(out.contains("Removal:"), "{out}");
    assert!(out.contains("Enter"), "{out}");
}

/// A notice takes a row of the same small block, and the keys still keep
/// their row instead of losing it to the message above them.
#[test]
fn test_notice_keeps_hint_visible() {
    let detail = with_preview(worktree_preview("alpha"), WorktreeLevel::Detail);
    detail
        .ui
        .worktree_pane
        .draft
        .borrow_mut()
        .set_notice("the worktree changed since the preview".to_string());
    let out = render_text(&detail, 80, 20);
    assert!(out.contains("d to remove"), "{out}");

    let confirm = with_preview(worktree_preview("alpha"), WorktreeLevel::Confirm);
    confirm
        .ui
        .worktree_pane
        .draft
        .borrow_mut()
        .set_notice("the worktree changed since the preview".to_string());
    let out = render_text(&confirm, 80, 20);
    assert!(out.contains("Removal:"), "{out}");
    assert!(out.contains("Esc to back"), "{out}");
}

/// With room for the notice, the answer sits above the key row.
#[test]
fn test_notice_shows_with_room() {
    let detail = with_preview(worktree_preview("alpha"), WorktreeLevel::Detail);
    detail
        .ui
        .worktree_pane
        .draft
        .borrow_mut()
        .set_notice("the worktree changed since the preview".to_string());
    let out = render_text(&detail, 80, 30);
    assert!(
        out.contains("Notice:   the worktree changed since the preview"),
        "{out}"
    );
    assert!(out.contains("d to remove"), "{out}");
}

/// A pane too short for the detail keeps its tail reachable: the last field is
/// off the block at the top, and scrolling to the end brings it into view. The
/// position is held at the last row that still fills the block, so a scroll
/// past the end does not leave the block blank.
#[test]
fn test_short_pane_scrolls_tail() {
    let detail = with_preview(worktree_preview("alpha"), WorktreeLevel::Detail);
    let out = render_text(&detail, 80, 10);
    assert!(
        !out.contains("Use:      free"),
        "the pane is short enough to clip the tail: {out}"
    );

    detail.ui.worktree_pane.scroll.set(200);
    let out = render_text(&detail, 80, 10);
    assert!(
        out.contains("Use:      free"),
        "the tail is reachable by scrolling: {out}"
    );
    assert!(
        !out.contains("Worktree: alpha"),
        "the top is scrolled out of the block: {out}"
    );
}

/// A block that fits reads as unscrolled, so a downward key at its end leaves
/// nothing to undo and an upward key starts from the top.
#[test]
fn test_fitting_block_keeps_top() {
    let detail = with_preview(worktree_preview("alpha"), WorktreeLevel::Detail);
    detail.ui.worktree_pane.scroll.set(200);
    let out = render_text(&detail, 80, 30);
    assert!(out.contains("Worktree: alpha"), "{out}");
    assert_eq!(
        detail.ui.worktree_pane.scroll.get(),
        0,
        "a block with room for every row holds no scroll"
    );
}

/// A path wider than the block wraps instead of losing its tail, so the
/// confirmation still names the directory it is about to take apart.
#[test]
fn test_wide_path_wraps() {
    let mut preview = worktree_preview("alpha");
    preview.path = "/some/repo/with-a-long-parent-name/tail-kept".to_string();
    let out = render_text(&with_preview(preview, WorktreeLevel::Confirm), 60, 24);
    assert!(out.contains("tail-kept)"), "{out}");
}

/// A removal with the host shows that it is under way, so the confirmation is
/// not silent while the answer is out.
#[test]
fn test_confirm_reports_progress() {
    let app = with_preview(worktree_preview("alpha"), WorktreeLevel::Confirm);
    assert!(
        app.ui
            .worktree_pane
            .draft
            .borrow_mut()
            .take_submission(RequestId(7))
            .is_some()
    );
    let out = render(&app);
    assert!(out.contains("Removing…"), "{out}");
}

/// A confirmation whose preview the host refused states the refusal and
/// offers neither the button nor the choice: nothing can be removed from a
/// page that does not know what a removal would discard.
#[test]
fn test_refused_preview_offers_nothing() {
    let app = pane_app();
    {
        let mut draft = app.ui.worktree_pane.draft.borrow_mut();
        draft.begin(&worktree_path("alpha"), WorktreeLevel::List, None);
        draft.set_notice("cannot read the worktree: not in a git work tree".to_string());
    }
    app.ui.worktree_pane.level.set(WorktreeLevel::Confirm);
    let out = render(&app);
    assert!(out.contains("cannot read the worktree"), "{out}");
    assert!(out.contains("r to read again"), "{out}");
    assert!(!out.contains("Remove worktree"), "{out}");
    assert!(!out.contains("branch choice"), "{out}");
}

/// A confirmation still waiting on its preview offers only the way back out,
/// since a choice narrowed to one button is not a choice.
#[test]
fn test_reading_preview_offers_escape() {
    let app = pane_app();
    {
        let mut draft = app.ui.worktree_pane.draft.borrow_mut();
        draft.begin(&worktree_path("alpha"), WorktreeLevel::List, None);
        draft.expect_preview(RequestId(4));
    }
    app.ui.worktree_pane.level.set(WorktreeLevel::Confirm);
    let out = render(&app);
    assert!(
        out.contains("Reading what a removal would discard"),
        "{out}"
    );
    assert!(out.contains("Esc to back"), "{out}");
    assert!(!out.contains("read again"), "{out}");
    assert!(!out.contains("Remove worktree"), "{out}");
}

/// Each merge answer reads as the fact it is: merged, ahead by a counted
/// number, unanswered against a base that is known, or no base at all.
#[test]
fn test_merge_label_names_state() {
    let mut merged = worktree_preview("alpha");
    merged.merged = true;
    let out = render(&with_preview(merged, WorktreeLevel::Confirm));
    assert!(out.contains("merged into dev"), "{out}");

    let mut single = worktree_preview("alpha");
    single.unmerged_commits = Some(1);
    let out = render(&with_preview(single, WorktreeLevel::Confirm));
    assert!(out.contains("unmerged: 1 commit ahead of dev"), "{out}");

    let mut unknown = worktree_preview("alpha");
    unknown.unmerged_commits = None;
    let out = render(&with_preview(unknown, WorktreeLevel::Confirm));
    assert!(out.contains("Git did not answer against dev"), "{out}");

    let mut no_base = worktree_preview("alpha");
    no_base.base_branch = None;
    let out = render(&with_preview(no_base, WorktreeLevel::Confirm));
    assert!(out.contains("base branch unknown"), "{out}");
    assert!(
        !out.contains("record"),
        "the copy must not name an internal store: {out}"
    );
}

/// A refusal lands on the list, above the rows the keys still act on, so the
/// user reads why where the gesture was made.
#[test]
fn test_list_shows_notice() {
    let app = pane_app();
    app.ui.worktree_pane.set_notice(format!(
        "{} is the repository's main working tree; it cannot be removed from here",
        worktree_path("alpha")
    ));
    let out = render(&app);
    assert!(out.contains("main working tree"), "{out}");
    assert!(
        out.contains("d to remove"),
        "the keys keep their row: {out}"
    );
}

/// A tree this product did not create keeps its branch, so the confirmation
/// says the directory goes and the branch stays, and offers no key that would
/// claim otherwise.
#[test]
fn test_foreign_branch_stays() {
    let mut preview = worktree_preview("alpha");
    preview.branch_deletable = false;
    let out = render(&with_preview(preview, WorktreeLevel::Confirm));
    assert!(
        out.contains("branch worktree-alpha kept in the repository"),
        "{out}"
    );
    assert!(!out.contains("b to branch choice"), "{out}");
}

/// A tree with nothing ignored says so rather than showing an empty suffix.
#[test]
fn test_ignored_summary_says_none() {
    let mut clean = worktree_preview("alpha");
    clean.ignored_build_dirs.clear();
    clean.ignored_unknown = 0;
    let out = render(&with_preview(clean, WorktreeLevel::Confirm));
    assert!(out.contains("Ignored:  none"), "{out}");
}

/// The main working tree cannot be entered or removed, so its detail names
/// that standing and offers only the key that goes back. Naming a verb there
/// would promise a host answer the host is certain to refuse.
#[test]
fn test_main_detail_offers_back() {
    let mut app = pane_app();
    let entry = &mut app.sessions.current.capabilities.worktree.entries[0];
    entry.is_main = true;
    entry.is_current = true;
    app.ui.worktree_pane.level.set(WorktreeLevel::Detail);
    let out = render(&app);
    assert!(
        out.contains("State:    main working tree (current)"),
        "{out}"
    );
    assert!(!out.contains("e to enter"), "{out}");
    assert!(!out.contains("d to remove"), "{out}");
    assert!(out.contains("Esc to back"), "{out}");
}

/// A main working tree that is not current cannot be entered either, so its
/// detail offers no verb. The enter key used to appear here, and every press
/// collected a refusal from the host.
#[test]
fn test_main_detail_hides_enter() {
    let mut app = pane_app();
    let entry = &mut app.sessions.current.capabilities.worktree.entries[0];
    entry.is_main = true;
    entry.is_current = false;
    app.ui.worktree_pane.level.set(WorktreeLevel::Detail);
    let out = render(&app);
    assert!(out.contains("State:    main working tree"), "{out}");
    assert!(!out.contains("e to enter"), "{out}");
    assert!(!out.contains("d to remove"), "{out}");
}

/// The list footer names only the verbs the selected row accepts: the main
/// working tree is entered by nobody and removed by nobody.
#[test]
fn test_list_skips_main_verbs() {
    let mut app = pane_app();
    let entry = &mut app.sessions.current.capabilities.worktree.entries[0];
    entry.is_main = true;
    entry.is_current = false;
    let out = render(&app);
    assert!(out.contains("Enter to open"), "{out}");
    assert!(!out.contains("e to enter"), "{out}");
    assert!(!out.contains("d to remove"), "{out}");
}

/// An empty list has no row to open, so the footer names no row action.
#[test]
fn test_empty_list_hides_keys() {
    let mut app = pane_app();
    app.sessions.current.capabilities.worktree.entries.clear();
    let out = render(&app);
    assert!(!out.contains("Enter to open"), "{out}");
    assert!(!out.contains("e to enter"), "{out}");
    assert!(!out.contains("d to remove"), "{out}");
    assert!(out.contains("Esc to close"), "{out}");
}

/// A linked tree the session works in cannot be entered again or removed, so
/// its detail hides both verbs while still naming the standing.
#[test]
fn test_current_detail_hides_verbs() {
    let mut app = pane_app();
    app.sessions.current.capabilities.worktree.entries[0].is_current = true;
    app.ui.worktree_pane.level.set(WorktreeLevel::Detail);
    let out = render(&app);
    assert!(out.contains("State:    current worktree"), "{out}");
    assert!(!out.contains("e to enter"), "{out}");
    assert!(!out.contains("d to remove"), "{out}");
}

/// A linked tree the session is not working in is the one the verbs act on.
#[test]
fn test_detail_linked_offers_verbs() {
    let app = pane_app();
    app.ui.worktree_pane.level.set(WorktreeLevel::Detail);
    let out = render(&app);
    assert!(out.contains("State:    linked worktree"), "{out}");
    assert!(out.contains("e to enter"), "{out}");
    assert!(out.contains("d to remove"), "{out}");
}

/// The list legend names both markers, and the dirty marker takes the warning
/// colour while the current marker keeps the ordinary one, so the two facts are
/// not read as the same state.
#[test]
fn test_list_legend_colours_markers() {
    let mut app = pane_app();
    app.sessions.current.capabilities.worktree.entries = vec![
        WorktreeEntry {
            path: "/some/repo".into(),
            head: "abcdef0".into(),
            branch: "main".into(),
            is_main: true,
            is_current: true,
            ..Default::default()
        },
        WorktreeEntry {
            path: "/some/repo/dirty".into(),
            head: "1234567".into(),
            branch: "feat".into(),
            dirty_summary: "1 modified".into(),
            ..Default::default()
        },
        WorktreeEntry {
            path: "/some/repo/other".into(),
            head: "7654321".into(),
            branch: "topic".into(),
            ..Default::default()
        },
    ];
    // The cursor row is drawn in the highlight style, which would hide the
    // marker colours this test is about, so the cursor sits on a plain row.
    app.ui.worktree_pane.list.cursor = 2;
    let out = render(&app);
    assert!(
        out.contains("worktrees — 3 listed (* current · ! dirty)"),
        "{out}"
    );
    let buf = render_buffer(&app, 80, 24);
    let current_row = row_of(&buf, "/some/repo ");
    let dirty_row = row_of(&buf, "/some/repo/dirty");
    assert!(
        row_has(&buf, current_row, |style| style.fg == Some(Color::Green)),
        "the current marker stays green"
    );
    assert!(
        row_has(&buf, dirty_row, |style| style.fg == Some(Color::Yellow)),
        "the dirty marker warns in yellow"
    );
}

/// The row of the drawing that holds a needle.
fn row_of(buf: &Buffer, needle: &str) -> u16 {
    (0..buf.area.height)
        .find(|&y| row_text(buf, y).contains(needle))
        .unwrap_or_else(|| panic!("{needle} is not on screen"))
}

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf[(x, y)].symbol())
        .collect::<String>()
}

fn row_has(buf: &Buffer, y: u16, matches: impl Fn(&Style) -> bool) -> bool {
    (0..buf.area.width).any(|x| matches(&buf[(x, y)].style()))
}
