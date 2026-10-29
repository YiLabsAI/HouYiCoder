//! Tests for what the sandbox pane draws on each tab and level.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::checkpoint::{
    CheckpointImage, CheckpointListView, CheckpointSide, CheckpointState, CheckpointTextView,
    CheckpointView, FenceState, SandboxBoundariesView, UnreadableView,
};
use ratatui::buffer::Buffer;

use super::*;
use crate::composition;
use crate::state::App;
use crate::state::{Pane, Screen};
use crate::test_harness::{render_buffer, render_text};

fn pane_app() -> App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Sandbox;
    app
}

fn bounds(fence: FenceState) -> SandboxBoundariesView {
    SandboxBoundariesView {
        fence,
        workspace_root: "/w/project".to_string(),
        writable_roots: vec!["/w/project".to_string(), "/tmp/scratch".to_string()],
        scratch_root: None,
    }
}

fn write(operation: &str, state: CheckpointState) -> CheckpointView {
    CheckpointView {
        operation: operation.to_string(),
        target: format!("/w/project/{operation}.rs"),
        state,
        written_ms: Some(1_000),
        before: CheckpointImage {
            present: false,
            hash: None,
            bytes: None,
        },
        after: CheckpointImage {
            present: true,
            hash: Some("9f2c".to_string()),
            bytes: Some(12),
        },
    }
}

/// A write whose target already existed, so putting it back writes bytes
/// rather than removing the file.
fn edited(operation: &str, state: CheckpointState) -> CheckpointView {
    CheckpointView {
        before: CheckpointImage {
            present: true,
            hash: Some("5e1d".to_string()),
            bytes: Some(8),
        },
        ..write(operation, state)
    }
}

fn listing() -> CheckpointListView {
    CheckpointListView {
        checkpoints: vec![
            write("first", CheckpointState::Applied),
            write("second", CheckpointState::Conflict),
            write("third", CheckpointState::Unverified),
        ],
        unreadable: vec![UnreadableView {
            operation: Some("fourth".to_string()),
            reason: "the record names no target".to_string(),
        }],
    }
}

fn texts(operation: &str, before: &str, after: &str) -> CheckpointTextView {
    CheckpointTextView {
        operation: operation.to_string(),
        target: format!("/w/project/{operation}.rs"),
        before: CheckpointSide::Text {
            text: before.to_string(),
            truncated: false,
        },
        after: CheckpointSide::Text {
            text: after.to_string(),
            truncated: false,
        },
    }
}

fn render(app: &App) -> String {
    render_text(app, 100, 30)
}

/// The row of the drawing that holds a needle, so a test can ask a question
/// about it.
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

fn is_cyan_bold(style: &Style) -> bool {
    style.fg == Some(Color::Cyan) && style.add_modifier.contains(Modifier::BOLD)
}

/// The sandbox tab reads as one answer: what the fence enforces, the root the
/// session was assembled over, and every root a write may land in. Its footer
/// names only the keys it acts on.
#[test]
fn test_boundaries_lists_roots() {
    let app = pane_app();
    app.ui
        .sandbox_pane
        .accept_boundaries(bounds(FenceState::Enforced));
    let out = render(&app);
    assert!(out.contains("[Sandbox]"), "{out}");
    assert!(
        out.contains("Status:    Enforced (writes outside the roots below are refused)"),
        "{out}"
    );
    assert!(out.contains("Workspace: /w/project"), "{out}");
    assert!(out.contains("/tmp/scratch"), "{out}");
    assert!(
        out.contains("Tab/Left/Right to switch tab · Esc to close"),
        "{out}"
    );
    assert!(!out.contains("permissions"), "{out}");
    assert!(!out.contains("read again"), "{out}");
}

/// The session's own scratch directory is a writable row even though the user
/// never granted it, and the row says whose it is: the child runs with it as
/// TMPDIR, so a boundary that hid it would understate what the fence opened.
#[test]
fn test_boundaries_show_scratch_row() {
    let app = pane_app();
    let mut view = bounds(FenceState::Enforced);
    view.scratch_root = Some("/var/folders/hy-tmp-501-0".to_string());
    app.ui.sandbox_pane.accept_boundaries(view);
    let out = render(&app);
    assert!(
        out.contains("/var/folders/hy-tmp-501-0 (session TMPDIR)"),
        "{out}"
    );
    assert!(out.contains("/tmp/scratch"), "{out}");
}

/// A fence the host could not set up states why. Reading as a healthy boundary
/// would tell the user writes are confined when nothing confines them.
#[test]
fn test_failed_fence_states_reason() {
    let app = pane_app();
    app.ui
        .sandbox_pane
        .accept_boundaries(bounds(FenceState::Failed("no kernel rule".to_string())));
    let out = render(&app);
    assert!(out.contains("Could not be set up: no kernel rule"), "{out}");
    assert!(!out.contains("Enforced"), "{out}");
}

#[test]
fn test_list_marks_states() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.accept_checkpoints(listing());
    let out = render(&app);
    assert!(out.contains("[Checkpoints]"), "{out}");
    assert!(out.contains("4 recorded"), "{out}");
    assert!(out.contains("active"), "{out}");
    assert!(out.contains("modified"), "{out}");
    assert!(out.contains("unverified"), "{out}");
    assert!(out.contains("/w/project/first.rs"), "{out}");
    assert!(out.contains("r to restore"), "{out}");
}

/// A record the host could not judge is listed with its reason, so the user
/// learns why it cannot be restored instead of finding it missing.
#[test]
fn test_list_names_unreadable() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.accept_checkpoints(listing());
    let out = render(&app);
    assert!(out.contains("unreadable"), "{out}");
    assert!(out.contains("fourth"), "{out}");
    assert!(out.contains("the record names no target"), "{out}");
}

/// An earlier write to the same file is superseded by a later one that still
/// stands, so the listing says where the target's bytes actually came from.
#[test]
fn test_later_write_supersedes_earlier() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    let mut first = edited("first", CheckpointState::Applied);
    first.target = "src/shared.rs".to_string();
    let mut second = edited("second", CheckpointState::Applied);
    second.target = "src/shared.rs".to_string();
    app.ui.sandbox_pane.accept_checkpoints(CheckpointListView {
        checkpoints: vec![first, second],
        unreadable: Vec::new(),
    });
    let out = render(&app);
    assert!(out.contains("superseded"), "{out}");
    assert_eq!(
        out.matches("active").count(),
        1,
        "only the later write is the one the target holds: {out}"
    );
}

/// A later write that was undone hands the file back, so it leaves the write
/// before it standing rather than superseding it.
#[test]
fn test_reverted_write_supersedes_nothing() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    let mut first = edited("first", CheckpointState::Applied);
    first.target = "src/shared.rs".to_string();
    let mut second = edited("second", CheckpointState::NotApplied);
    second.target = "src/shared.rs".to_string();
    app.ui.sandbox_pane.accept_checkpoints(CheckpointListView {
        checkpoints: vec![first, second],
        unreadable: Vec::new(),
    });
    let out = render(&app);
    assert!(out.contains("active"), "{out}");
    assert!(out.contains("reverted"), "{out}");
    assert!(!out.contains("superseded"), "{out}");
}

/// An empty listing offers no key that needs a row: selecting, opening, and
/// restoring all act on a record, and there is none to act on. The two keys
/// that still work stay named.
#[test]
fn test_empty_list_hides_keys() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.accept_checkpoints(CheckpointListView {
        checkpoints: Vec::new(),
        unreadable: Vec::new(),
    });
    let out = render(&app);
    assert!(out.contains("0 recorded"), "{out}");
    assert!(!out.contains("Up/Down to select"), "{out}");
    assert!(!out.contains("Enter to open"), "{out}");
    assert!(!out.contains("r to restore"), "{out}");
    assert!(out.contains("Left/Right to switch tab"), "{out}");
    assert!(out.contains("Esc to close"), "{out}");
}

/// A record the host could not judge is not one the row keys can act on, so
/// the list names none of them while that row is under the cursor. The keys
/// that act on the pane itself stay named.
#[test]
fn test_unreadable_row_hides_keys() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.accept_checkpoints(listing());
    app.ui.sandbox_pane.cursor.set(3);
    let out = render(&app);
    assert!(!out.contains("r to restore"), "{out}");
    assert!(!out.contains("Up/Down to select"), "{out}");
    assert!(!out.contains("Enter to open"), "{out}");
    assert!(out.contains("Left/Right to switch tab"), "{out}");
    assert!(out.contains("Esc to close"), "{out}");
}

/// The row under the cursor keeps the colour of its state; the selection adds
/// weight to the target alone, so a changed row is not recoloured into
/// looking healthy.
#[test]
fn test_selected_row_keeps_colour() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.accept_checkpoints(CheckpointListView {
        checkpoints: vec![
            write("first", CheckpointState::Applied),
            write("second", CheckpointState::Conflict),
        ],
        unreadable: Vec::new(),
    });
    app.ui.sandbox_pane.cursor.set(1);
    let buf = render_buffer(&app, 100, 30);
    let selected = row_of(&buf, "second.rs");
    assert!(row_has(&buf, selected, |style| style.fg == Some(Color::Yellow)));
    assert!(row_has(&buf, selected, is_cyan_bold));
    let other = row_of(&buf, "first.rs");
    assert!(row_has(&buf, other, |style| style.fg == Some(Color::Green)));
    assert!(!row_has(&buf, other, is_cyan_bold));
}

/// The detail compares the two sides line by line, so the user reads what the
/// write changed instead of the names the bytes are stored under.
#[test]
fn test_detail_compares_sides() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.accept_checkpoints(listing());
    app.ui.sandbox_pane.set_level(CheckpointLevel::Detail);
    app.ui
        .sandbox_pane
        .accept_texts(texts("first", "old line", "new line"));
    let out = render(&app);
    assert!(out.contains("first.rs"), "the title names the file: {out}");
    assert!(out.contains("-old line"), "{out}");
    assert!(out.contains("+new line"), "{out}");
    assert!(out.contains("State:      active"), "{out}");
    assert!(out.contains("/w/project/first.rs"), "{out}");
    assert!(!out.contains("9f2c"), "the stored name stays hidden: {out}");
}

/// A side the host could not read is named, and a side it held more bytes
/// back from says so, rather than either reading as an empty file.
#[test]
fn test_detail_names_unreadable_sides() {
    let cases = [
        (
            CheckpointSide::NotKept,
            "Before: the record kept none of these bytes",
        ),
        (CheckpointSide::NotText, "Before: these bytes are not text"),
        (
            CheckpointSide::Text {
                text: "cut".to_string(),
                truncated: true,
            },
            "Before: the file is larger than this view holds, so the sides are not compared",
        ),
    ];
    for (before, needle) in cases {
        let app = pane_app();
        app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
        app.ui.sandbox_pane.accept_checkpoints(listing());
        app.ui.sandbox_pane.set_level(CheckpointLevel::Detail);
        app.ui.sandbox_pane.accept_texts(CheckpointTextView {
            operation: "first".to_string(),
            target: "/w/project/first.rs".to_string(),
            before,
            after: CheckpointSide::Text {
                text: "new".to_string(),
                truncated: false,
            },
        });
        let out = render(&app);
        assert!(out.contains(needle), "{needle}: {out}");
    }
}

/// A comparison is drawn only when both sides can be read whole. A cut side
/// would show its own cut as a change, and a side the record kept none of
/// would make every line of the other side read as if it had just arrived.
#[test]
fn test_uncomparable_skips_diff() {
    let cases = [
        (
            CheckpointSide::NotKept,
            CheckpointSide::Text {
                text: "new line".to_string(),
                truncated: false,
            },
            "+new line",
        ),
        (
            CheckpointSide::Text {
                text: "old line".to_string(),
                truncated: true,
            },
            CheckpointSide::Text {
                text: "new line".to_string(),
                truncated: false,
            },
            "-old line",
        ),
    ];
    for (before, after, forbidden) in cases {
        let app = pane_app();
        app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
        app.ui.sandbox_pane.accept_checkpoints(listing());
        app.ui.sandbox_pane.set_level(CheckpointLevel::Detail);
        app.ui.sandbox_pane.accept_texts(CheckpointTextView {
            operation: "first".to_string(),
            target: "/w/project/first.rs".to_string(),
            before,
            after,
        });
        let out = render(&app);
        assert!(
            !out.contains(forbidden),
            "a side that cannot be compared in full is not compared: {out}"
        );
    }
}

/// A record whose earlier bytes were never kept has nothing a restore could
/// put back, so neither the listing nor the detail offers the key. The state
/// row already names why.
#[test]
fn test_unverified_hides_restore() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.accept_checkpoints(listing());
    app.ui.sandbox_pane.cursor.set(2);
    let out = render(&app);
    assert!(out.contains("unverified"), "{out}");
    assert!(!out.contains("r to restore"), "{out}");

    app.ui.sandbox_pane.set_level(CheckpointLevel::Detail);
    let out = render(&app);
    assert!(out.contains("unverified"), "{out}");
    assert!(!out.contains("r to restore"), "{out}");
}

/// The detail of an unreadable record offers no restore, because there is
/// nothing the host could put back.
#[test]
fn test_detail_offers_no_restore() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.accept_checkpoints(listing());
    app.ui.sandbox_pane.cursor.set(3);
    app.ui.sandbox_pane.set_level(CheckpointLevel::Detail);
    let out = render(&app);
    assert!(
        out.contains("Unreadable: the record names no target"),
        "{out}"
    );
    assert!(!out.contains("r to restore"), "{out}");
}

/// The confirmation starts on the button that changes nothing, and says what a
/// restore does to a target that something else changed.
#[test]
fn test_confirm_starts_on_cancel() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.accept_checkpoints(CheckpointListView {
        checkpoints: vec![edited("second", CheckpointState::Conflict)],
        unreadable: Vec::new(),
    });
    app.ui.sandbox_pane.begin_confirm();
    let out = render(&app);
    assert!(out.contains("[Cancel]"), "{out}");
    assert!(out.contains("Restore checkpoint"), "{out}");
    assert!(!out.contains("[Restore checkpoint]"), "{out}");
    assert!(out.contains("keeps its bytes"), "{out}");
    assert!(out.contains("copy beside it"), "{out}");
    assert!(!out.contains("5e1d"), "the stored name stays hidden: {out}");
}

/// A write that created its target has nothing earlier to put back. The host
/// removes the file for that record, so the confirmation says so instead of
/// promising bytes that were never kept.
#[test]
fn test_confirm_names_removal() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.accept_checkpoints(listing());
    app.ui.sandbox_pane.begin_confirm();
    let out = render(&app);
    assert!(out.contains("removes the file this write created"), "{out}");
    assert!(
        !out.contains("puts the bytes the write found back"),
        "{out}"
    );
}

/// A refusal sits above the rows, where the key that did nothing was pressed,
/// and the rows stay readable underneath it.
#[test]
fn test_list_shows_notice() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.accept_checkpoints(listing());
    app.ui.sandbox_pane.set_notice(
        "checkpoint: the earlier bytes of /w/project/third.rs were never kept".to_string(),
    );
    let out = render(&app);
    assert!(
        out.contains("the earlier bytes of /w/project/third.rs were never kept"),
        "{out}"
    );
    assert!(out.contains("/w/project/first.rs"), "{out}");
}

#[test]
fn test_empty_list_says_so() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.accept_checkpoints(CheckpointListView {
        checkpoints: Vec::new(),
        unreadable: Vec::new(),
    });
    let out = render(&app);
    assert!(
        out.contains("(no file writes were recorded in this session)"),
        "{out}"
    );
}

/// A 40-column terminal still names both tabs, the state of each row, the file
/// the row targets, and the key that leaves. The target gives up its leading
/// directories to the state column and the cursor marker, never the file name
/// that tells one row from the next.
#[test]
fn test_columns_keep_row_identity() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    let mut view = write("deep", CheckpointState::Applied);
    view.target =
        "/work/repo/.claude/worktrees/s36-t3/crates/houyicoder-tui/src/view/sandbox_pane.rs"
            .to_string();
    app.ui.sandbox_pane.accept_checkpoints(CheckpointListView {
        checkpoints: vec![view],
        unreadable: Vec::new(),
    });
    for width in [40, 80, 120] {
        let out = render_text(&app, width, 30);
        assert!(out.contains("Sandbox"), "{width}: {out}");
        assert!(out.contains("[Checkpoints]"), "{width}: {out}");
        assert!(out.contains("active"), "{width}: {out}");
        assert!(out.contains("sandbox_pane.rs"), "{width}: {out}");
        assert!(out.contains("Esc"), "{width}: {out}");
    }
}

/// A write that never landed states that in the same column as the age of one
/// that did, so the two targets still start at the same place.
#[test]
fn test_rows_share_target_column() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    let mut landed = write("landed", CheckpointState::Applied);
    landed.target = "src/landed.rs".to_string();
    let mut never = write("never", CheckpointState::Applied);
    never.target = "src/never.rs".to_string();
    never.written_ms = None;
    app.ui.sandbox_pane.accept_checkpoints(CheckpointListView {
        checkpoints: vec![landed, never],
        unreadable: Vec::new(),
    });
    let out = render_text(&app, 100, 30);
    let column = |target: &str| {
        let line = out
            .lines()
            .find(|line| line.contains(target))
            .unwrap_or_else(|| panic!("{target} is not on screen:\n{out}"));
        let at = line.find(target).unwrap_or_default();
        line[..at].chars().count()
    };
    assert!(out.contains("never landed"), "{out}");
    assert_eq!(column("src/landed.rs"), column("src/never.rs"), "{out}");
}

/// A target whose glyphs are wider than one column, compared against a long
/// side. The detail still reaches the last line when scrolled to the end,
/// because the wrap counts the columns a wide glyph takes rather than the
/// characters it has.
#[test]
fn test_wide_glyphs_reach_end() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    let mut view = write("wide", CheckpointState::Applied);
    view.target = format!("/w/{}", "报告📁".repeat(8));
    app.ui.sandbox_pane.accept_checkpoints(CheckpointListView {
        checkpoints: vec![view],
        unreadable: Vec::new(),
    });
    app.ui.sandbox_pane.set_level(CheckpointLevel::Detail);
    app.ui.sandbox_pane.accept_texts(CheckpointTextView {
        operation: "wide".to_string(),
        target: format!("/w/{}", "报告📁".repeat(8)),
        before: CheckpointSide::Text {
            text: "报告📁the last line before".to_string(),
            truncated: false,
        },
        after: CheckpointSide::Text {
            text: "报告📁the last line after".to_string(),
            truncated: false,
        },
    });
    app.ui.sandbox_pane.scroll.set(u16::MAX);
    let out = render_text(&app, 30, 12);
    assert!(out.contains("the last line after"), "{out}");
}

/// The scroll hint belongs to the key row only while the block is taller than
/// the room it has, so a key that would move nothing is not offered.
#[test]
fn test_scroll_hint_when_needed() {
    let app = pane_app();
    app.ui
        .sandbox_pane
        .accept_boundaries(bounds(FenceState::Enforced));
    let short = render(&app);
    assert!(!short.contains("Up/Down to scroll"), "{short}");

    app.ui
        .sandbox_pane
        .accept_boundaries(SandboxBoundariesView {
            fence: FenceState::Enforced,
            workspace_root: "/w/project".to_string(),
            writable_roots: (0..40).map(|i| format!("/w/root-{i}")).collect(),
            scratch_root: None,
        });
    let tall = render(&app);
    assert!(tall.contains("Up/Down to scroll"), "{tall}");
}

/// A tab whose read is still out says so, and one that never got an answer
/// says how to ask again now that no key does it.
#[test]
fn test_pending_read_names_retry() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.expect_list(RequestId(7));
    let out = render(&app);
    assert!(
        out.contains("Reading the recorded writes from the host…"),
        "{out}"
    );

    app.ui.sandbox_pane.reset();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    let out = render(&app);
    assert!(out.contains("reopening the pane reads again"), "{out}");
}
