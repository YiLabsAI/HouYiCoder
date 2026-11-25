//! State transitions for the /lsp pane: the reply that settles a pending
//! action, the cursor and detail levels, the scroll clamp, and the row
//! gating the key handler consults.

use std::time::Instant;

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::lsp::{LspConfigScope, LspServerState, LspServerStatusView};

use super::LSP_REFRESH_BUDGET;
use super::LSP_REFRESH_INTERVAL;
use super::LspPaneState;

fn row(name: &str, state: LspServerState, locked: bool) -> LspServerStatusView {
    LspServerStatusView {
        name: name.into(),
        scope: LspConfigScope::User,
        state,
        locked,
        command_line: "rust-analyzer".into(),
        extensions: vec!["rs".into()],
        open_documents: 1,
        diagnostic_count: 0,
        recent_stderr: Vec::new(),
    }
}

fn rows() -> Vec<LspServerStatusView> {
    vec![
        row("alpha", LspServerState::Running, false),
        row(
            "beta",
            LspServerState::Failed {
                error: "the server exited".into(),
            },
            false,
        ),
        row(
            "gated",
            LspServerState::Disabled {
                reason: "the workspace is not trusted".into(),
            },
            true,
        ),
    ]
}

/// A reply whose request does not match the pending one changes nothing, so
/// a late answer cannot overwrite a newer action's rows.
#[test]
fn test_apply_status_stale_ignored() {
    let mut pane = LspPaneState::default();
    pane.mark_pending(RequestId(7));
    pane.apply_status(RequestId(8), rows());
    assert!(pane.rows.is_empty(), "a stale reply changes nothing");
    assert!(pane.pending.is_some(), "the pending action stands");
}

/// A matching reply settles the pending action, hands back its receipt, and
/// clamps the cursor into the new list.
#[test]
fn test_apply_status_settles() {
    let mut pane = LspPaneState::default();
    pane.mark_pending_toggle(RequestId(7), "alpha".into(), false, false);
    pane.cursor = 9;
    let receipt = pane.apply_status(RequestId(7), rows());
    let receipt = receipt.expect("the toggle's receipt comes back");
    assert_eq!(receipt.name, "alpha");
    assert!(!receipt.enabled);
    assert_eq!(pane.rows.len(), 3);
    assert_eq!(pane.cursor, 2, "the cursor clamps into the list");
    assert!(pane.pending.is_none());
}

/// An error reply for the pane's own request settles the pending action and
/// keeps the rows it already holds.
#[test]
fn test_fail_pending_keeps_rows() {
    let mut pane = LspPaneState::default();
    pane.mark_pending(RequestId(3));
    pane.apply_status(RequestId(3), rows());
    pane.mark_pending_toggle(RequestId(4), "beta".into(), true, false);
    assert!(pane.fail_pending(RequestId(4)));
    assert!(pane.pending.is_none());
    assert_eq!(pane.rows.len(), 3, "the rows stand");
    assert!(
        !pane.fail_pending(RequestId(4)),
        "a second settle is not the pane's"
    );
}

/// The row gating follows the state and the lock: a locked row refuses both
/// action keys, a user-disabled row refuses the restart, and a live row
/// allows both.
#[test]
fn test_row_gating_follows_state() {
    let mut pane = LspPaneState::default();
    pane.mark_pending(RequestId(1));
    pane.apply_status(RequestId(1), rows());
    pane.cursor = 0;
    assert!(pane.allows_restart());
    assert!(pane.allows_toggle());
    pane.cursor = 1;
    assert!(pane.allows_restart());
    assert!(pane.allows_toggle());
    pane.cursor = 2;
    assert!(!pane.allows_restart());
    assert!(!pane.allows_toggle());
    pane.rows[1].state = LspServerState::Disabled {
        reason: "disabled by the user".into(),
    };
    pane.cursor = 1;
    assert!(!pane.allows_restart());
    assert!(pane.allows_toggle());
}

/// The receipt says whether the choice outlives the session: a scope that
/// does not persist reads as this session only, and a user-scope choice
/// reads as the plain change.
#[test]
fn test_toggle_receipt_line() {
    let session = super::LspToggleReceipt {
        name: "rust".into(),
        enabled: false,
        session_only: true,
    };
    assert_eq!(session.line(), "lsp: rust disabled (this session only)");
    let persistent = super::LspToggleReceipt {
        name: "rust".into(),
        enabled: true,
        session_only: false,
    };
    assert_eq!(persistent.line(), "lsp: rust enabled");
}

/// The cursor clamps at both ends and collapses to zero on an empty list.
#[test]
fn test_cursor_clamps() {
    let mut pane = LspPaneState::default();
    pane.mark_pending(RequestId(1));
    pane.apply_status(RequestId(1), rows());
    pane.move_cursor(-5);
    assert_eq!(pane.cursor, 0);
    pane.move_cursor(5);
    assert_eq!(pane.cursor, 2);
    pane.rows.clear();
    pane.move_cursor(1);
    assert_eq!(pane.cursor, 0);
}

/// The detail opens only from the list with a row under the cursor, resets
/// its scroll on open, and returns without disturbing the rows.
#[test]
fn test_detail_levels() {
    let mut pane = LspPaneState::default();
    pane.mark_pending(RequestId(1));
    pane.apply_status(RequestId(1), rows());
    pane.open_detail();
    assert_eq!(pane.level, 1);
    pane.close_detail();
    assert_eq!(pane.level, 0);
    assert_eq!(pane.rows.len(), 3, "the rows stand through both levels");
    pane.rows.clear();
    pane.open_detail();
    assert_eq!(pane.level, 0, "an empty list has nothing to open");
}

/// The detail scroll clamps to the maximum the render published, and a
/// shorter body pulls the maximum down with it.
#[test]
fn test_detail_scroll_clamps() {
    let pane = LspPaneState::default();
    pane.set_detail_max_offset(10);
    pane.scroll_detail(6);
    assert_eq!(pane.detail_offset(), 6);
    pane.scroll_detail(6);
    assert_eq!(pane.detail_offset(), 10, "the scroll stops at the maximum");
    pane.scroll_detail(-100);
    assert_eq!(pane.detail_offset(), 0);
    pane.scroll_detail(8);
    pane.set_detail_max_offset(3);
    assert_eq!(
        pane.detail_offset(),
        3,
        "a shorter body pulls the offset down"
    );
}

/// A session switch clears everything, so the next session's pane starts
/// from its own first reply.
#[test]
fn test_reset_clears() {
    let mut pane = LspPaneState::default();
    pane.mark_pending(RequestId(1));
    pane.apply_status(RequestId(1), rows());
    pane.mark_pending(RequestId(2));
    pane.cursor = 2;
    pane.level = 1;
    pane.scroll_detail(4);
    pane.reset();
    assert!(pane.rows.is_empty());
    assert!(pane.pending.is_none());
    assert_eq!(pane.cursor, 0);
    assert_eq!(pane.level, 0);
    assert_eq!(pane.detail_offset(), 0);
    assert!(!pane.refresh_armed(), "no settling poll survives a reset");
}

/// A reply that leaves a row starting or indexing arms one follow-up ask,
/// not before the interval; a reply with every row at rest disarms it.
#[test]
fn test_settling_poll_arms() {
    let mut pane = LspPaneState::default();
    pane.mark_pending(RequestId(1));
    pane.apply_status(
        RequestId(1),
        vec![row("alpha", LspServerState::Starting, false)],
    );
    assert!(pane.refresh_armed(), "a starting row arms the follow-up");
    let now = Instant::now();
    assert!(
        !pane.take_due_refresh(now),
        "the follow-up waits out its interval"
    );
    let later = now + LSP_REFRESH_INTERVAL;
    assert!(pane.take_due_refresh(later), "the due ask fires once");
    assert!(!pane.refresh_armed(), "one ask at a time");
    pane.mark_settling_pending(RequestId(2));
    pane.apply_status(
        RequestId(2),
        vec![row("alpha", LspServerState::Running, false)],
    );
    assert!(!pane.refresh_armed(), "a settled row disarms the follow-up");
}

/// A restart's reply is answered as the fresh launch begins, so the pane
/// asks once more even though the reply reads settled; only that follow-up
/// can show the new connection.
#[test]
fn test_restart_expects_settle() {
    let mut pane = LspPaneState::default();
    pane.mark_pending_restart(RequestId(9));
    pane.apply_status(
        RequestId(9),
        vec![row("alpha", LspServerState::Idle, false)],
    );
    assert!(
        pane.refresh_armed(),
        "a restart arms one follow-up despite a settled reply"
    );
}

/// The follow-up budget bounds one action: a row that never settles spends
/// the asks and stops, so a wedged indexing pass is not polled forever.
#[test]
fn test_settling_poll_budget() {
    let mut pane = LspPaneState::default();
    pane.mark_pending(RequestId(1));
    let indexing = || {
        row(
            "wedged",
            LspServerState::Indexing {
                message: Some("crates".into()),
            },
            false,
        )
    };
    pane.apply_status(RequestId(1), vec![indexing()]);
    let mut now = Instant::now() + LSP_REFRESH_INTERVAL;
    for _ in 0..LSP_REFRESH_BUDGET {
        assert!(pane.take_due_refresh(now));
        now += LSP_REFRESH_INTERVAL;
        // Every poll's reply still shows the row settling.
        pane.mark_settling_pending(RequestId(2));
        pane.apply_status(RequestId(2), vec![indexing()]);
    }
    assert!(!pane.refresh_armed(), "the budget is spent");
    assert!(
        !pane.take_due_refresh(now + LSP_REFRESH_INTERVAL),
        "a spent budget asks nothing"
    );
}
