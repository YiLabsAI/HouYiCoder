//! Tests for what the sandbox pane draws on each tab and level.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::checkpoint::{
    CheckpointImage, CheckpointListView, CheckpointState, CheckpointView, FenceState,
    SandboxBoundariesView, UnreadableView,
};

use super::*;
use crate::composition;
use crate::state::App;
use crate::state::{Pane, Screen};
use crate::test_harness::render_text;

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

fn render(app: &App) -> String {
    render_text(app, 100, 30)
}

/// The boundaries tab names the fence's state and every root a write may land
/// in, so the pane answers what confines the session rather than naming the
/// sandbox mode.
#[test]
fn test_boundaries_lists_roots() {
    let app = pane_app();
    app.ui
        .sandbox_pane
        .accept_boundaries(bounds(FenceState::Enforced));
    let out = render(&app);
    assert!(out.contains("[Boundaries]"), "{out}");
    assert!(out.contains("Fence:     enforced"), "{out}");
    assert!(out.contains("Workspace: /w/project"), "{out}");
    assert!(out.contains("/tmp/scratch"), "{out}");
    assert!(out.contains("Left/Right to switch tab"), "{out}");
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
    assert!(out.contains("could not be set up: no kernel rule"), "{out}");
    assert!(!out.contains("Fence:     enforced:"), "{out}");
}

#[test]
fn test_list_marks_states() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.accept_checkpoints(listing());
    let out = render(&app);
    assert!(out.contains("[Checkpoints]"), "{out}");
    assert!(out.contains("4 recorded"), "{out}");
    assert!(out.contains("ready"), "{out}");
    assert!(out.contains("conflict"), "{out}");
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

#[test]
fn test_detail_shows_images() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.accept_checkpoints(listing());
    app.ui.sandbox_pane.set_level(CheckpointLevel::Detail);
    let out = render(&app);
    assert!(out.contains("Checkpoint: first"), "{out}");
    assert!(out.contains("Target:     /w/project/first.rs"), "{out}");
    assert!(out.contains("Before:     absent"), "{out}");
    assert!(out.contains("After:      12 bytes, kept as 9f2c"), "{out}");
    assert!(out.contains("r to restore"), "{out}");
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
    assert!(out.contains("goes to a copy"), "{out}");
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
    assert!(!out.contains("puts the bytes from Before back"), "{out}");
}

/// A refusal sits above the rows, where the key that did nothing was pressed,
/// and the rows stay readable underneath it.
#[test]
fn test_list_shows_notice() {
    let app = pane_app();
    app.ui.sandbox_pane.tab.set(SandboxTab::Checkpoints);
    app.ui.sandbox_pane.accept_checkpoints(listing());
    app.ui
        .sandbox_pane
        .set_notice("checkpoint: the earlier bytes of third were never kept".to_string());
    let out = render(&app);
    assert!(
        out.contains("the earlier bytes of third were never kept"),
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
        assert!(out.contains("Boundaries"), "{width}: {out}");
        assert!(out.contains("[Checkpoints]"), "{width}: {out}");
        assert!(out.contains("ready"), "{width}: {out}");
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

/// A target whose glyphs are wider than one column. The detail still reaches
/// its last line when scrolled to the end, because the wrap counts the columns
/// a wide glyph takes rather than the characters it has.
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
    app.ui.sandbox_pane.scroll.set(u16::MAX);
    let out = render_text(&app, 30, 12);
    assert!(out.contains("9f2c"), "{out}");
}

/// A tab whose read is still out says so, and one that never got an answer
/// names the key that tries again.
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
    assert!(out.contains("r reads the checkpoints again"), "{out}");
}
