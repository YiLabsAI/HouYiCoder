//! Tests for the sandbox pane's own state: where the cursor is held, which
//! request a screen waits on, and what leaving a level resets.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::checkpoint::{
    CheckpointImage, CheckpointListView, CheckpointState, CheckpointView, FenceState,
    SandboxBoundariesView, UnreadableView,
};

use super::{CheckpointLevel, RestoreFocus, SandboxPaneState, SandboxTab};

fn state() -> SandboxPaneState {
    SandboxPaneState::default()
}

fn write(operation: &str, target: &str) -> CheckpointView {
    CheckpointView {
        operation: operation.to_string(),
        target: target.to_string(),
        state: CheckpointState::Applied,
        written_ms: Some(1_000),
        before: CheckpointImage {
            present: false,
            hash: None,
            bytes: None,
        },
        after: CheckpointImage {
            present: true,
            hash: Some("abc".to_string()),
            bytes: Some(3),
        },
    }
}

fn list(writes: usize, unreadable: usize) -> CheckpointListView {
    CheckpointListView {
        checkpoints: (0..writes)
            .map(|i| write(&format!("op-{i}"), "/w/file"))
            .collect(),
        unreadable: (0..unreadable)
            .map(|i| UnreadableView {
                operation: Some(format!("op-{i}")),
                reason: "the record could not be read".to_string(),
            })
            .collect(),
    }
}

#[test]
fn test_cursor_stays_inside_list() {
    let pane = state();
    pane.accept_checkpoints(list(3, 0));
    pane.move_cursor(1, 3);
    assert_eq!(pane.cursor.get(), 1);
    pane.move_cursor(10, 3);
    assert_eq!(pane.cursor.get(), 2, "the cursor stops at the last row");
    pane.move_cursor(-10, 3);
    assert_eq!(pane.cursor.get(), 0, "the cursor stops at the first row");
    pane.move_cursor(1, 0);
    assert_eq!(pane.cursor.get(), 0, "an empty listing holds it at zero");
}

/// Unreadable writes are rows too, so the cursor counts them and a selection
/// can land on one to read why the host could not judge it.
#[test]
fn test_list_clamps_cursor() {
    let pane = state();
    pane.cursor.set(9);
    pane.accept_checkpoints(list(1, 2));
    assert_eq!(pane.cursor.get(), 2);
}

#[test]
fn test_tab_switch_starts_list() {
    let pane = state();
    pane.set_level(CheckpointLevel::Detail);
    pane.scroll_by(4);
    pane.set_tab(SandboxTab::Checkpoints);
    assert_eq!(pane.level.get(), CheckpointLevel::List);
    assert_eq!(pane.scroll.get(), 0);
    assert_eq!(pane.tab.get(), SandboxTab::Checkpoints);
}

#[test]
fn test_restore_backs_to_detail() {
    let pane = state();
    pane.set_level(CheckpointLevel::Detail);
    pane.toggle_focus();
    pane.begin_confirm();
    assert_eq!(pane.level.get(), CheckpointLevel::Restore);
    assert_eq!(
        pane.focus.get(),
        RestoreFocus::Cancel,
        "the confirmation starts on the button that changes nothing"
    );
    assert_eq!(pane.restore_from(), CheckpointLevel::Detail);

    pane.set_level(CheckpointLevel::List);
    pane.begin_confirm();
    assert_eq!(
        pane.restore_from(),
        CheckpointLevel::List,
        "a confirmation opened from the list returns to the list"
    );
}

/// One gesture costs one request: a restore already with the host takes no
/// second one, whatever the user presses.
#[test]
fn test_second_restore_refused() {
    let pane = state();
    assert!(pane.begin_restore(RequestId(4)));
    assert!(!pane.begin_restore(RequestId(5)));
    assert_eq!(pane.restore_request(), Some(RequestId(4)));
}

/// Ending a restore frees the gesture for a retry and says nothing itself:
/// the outcome is reported where it outlives the pane.
#[test]
fn test_end_clears_restore() {
    let pane = state();
    assert!(pane.begin_restore(RequestId(4)));
    pane.end_restore();
    assert_eq!(pane.restore_request(), None);
    assert_eq!(pane.notice(), None);
    assert!(pane.begin_restore(RequestId(5)), "a retry goes out");
}

/// A fresh answer clears the failure it replaces, so a re-read that worked
/// stops showing the error the earlier one reported.
#[test]
fn test_list_clears_notice() {
    let pane = state();
    pane.set_notice("cannot list the checkpoints: broken pipe".to_string());
    pane.accept_checkpoints(list(1, 0));
    assert_eq!(pane.notice(), None);
}

#[test]
fn test_boundaries_clears_notice() {
    let pane = state();
    pane.set_notice("cannot read the boundaries: broken pipe".to_string());
    pane.accept_boundaries(SandboxBoundariesView {
        fence: FenceState::Enforced,
        workspace_root: "/w".to_string(),
        writable_roots: vec!["/w".to_string()],
    });
    assert_eq!(pane.notice(), None);
}

#[test]
fn test_reset_drops_answers() {
    let pane = state();
    pane.accept_boundaries(SandboxBoundariesView {
        fence: FenceState::Enforced,
        workspace_root: "/w".to_string(),
        writable_roots: vec!["/w".to_string()],
    });
    pane.expect_list(RequestId(2));
    pane.set_tab(SandboxTab::Checkpoints);
    pane.reset();
    assert_eq!(pane.tab.get(), SandboxTab::Boundaries);
    assert!(pane.boundaries().is_none());
    assert!(!pane.reading_checkpoints());
    assert_eq!(pane.cursor.get(), 0);
}
