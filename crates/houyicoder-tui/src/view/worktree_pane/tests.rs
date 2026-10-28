//! Tests for what the worktree pane draws at each level.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::worktree::WorktreePreviewView;

use super::*;
use crate::composition;
use crate::composition::WorktreeEntry;
use crate::state::App;
use crate::state::{Pane, Screen};
use crate::test_harness::render_text;
use crate::test_harness::worktree_path;
use crate::test_harness::worktree_preview;

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
        draft.begin(&worktree_path("alpha"), WorktreeLevel::List);
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
    assert!(out.contains("the record names no base branch"), "{out}");
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
