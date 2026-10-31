//! The Config tab's row list: what each row reads, and which rows act.

use super::*;
use crate::composition;

fn app() -> App {
    composition::app()
}

/// Every row prints its label capitalized, so the column reads as one kind of
/// thing rather than a mix of a capitalized first row and lower-case rest.
#[test]
fn test_labels_are_capitalized() {
    for row in CONFIG_ROWS {
        let label = row_label(row);
        let first = label.chars().next().expect("a label is not empty");
        assert!(
            first.is_uppercase(),
            "the label starts capitalized: {label}"
        );
    }
}

/// The rows that show state the session does not own take no press, so they
/// print no hint. A hint on such a row promises a change the pane cannot make.
#[test]
fn test_read_only_rows_inert() {
    assert!(
        !row_changes(ConfigRow::Model),
        "the model picker is its own surface"
    );
    assert!(!row_changes(ConfigRow::Sandbox), "the sandbox is fixed");
    for row in [
        ConfigRow::PermissionMode,
        ConfigRow::AutoApproveTimer,
        ConfigRow::AutoMemory,
        ConfigRow::AutoDream,
    ] {
        assert!(row_changes(row), "{row:?} acts on Enter");
    }
}

/// The timer row reads the delay the approval card arms its countdown from,
/// so a change on this row is visible on the next card.
#[test]
fn test_timer_row_reflects_delay() {
    let mut app = app();
    app.sessions
        .current
        .capabilities
        .permission
        .auto_approve_delay_secs = 15;
    assert_eq!(row_value(&app, ConfigRow::AutoApproveTimer), "15s");
    app.sessions
        .current
        .capabilities
        .permission
        .auto_approve_delay_secs = 0;
    assert_eq!(
        row_value(&app, ConfigRow::AutoApproveTimer),
        "off",
        "zero means the countdown is off, not a zero-second wait"
    );
}

/// Every timer step is reachable from every other one: the ring the Enter key
/// walks has no dead value in it.
#[test]
fn test_timer_steps_distinct() {
    let mut seen: Vec<u64> = TIMER_STEPS.to_vec();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen.len(),
        TIMER_STEPS.len(),
        "a duplicated step would look like a key that did nothing"
    );
    assert!(
        seen.contains(&0),
        "the ring includes off, so the countdown can be turned off from here"
    );
}

/// The two recap rows read from the settings snapshot, so the Config tab shows
/// the same switch and window the host reads.
#[test]
fn test_recap_rows_render() {
    let mut app = composition::app();
    {
        let mut snap = app.snapshot_or_stub();
        snap.recap_enabled = true;
        snap.recap_idle_secs = 600;
        app.sessions.current.capabilities.status.status_cache = Some(snap);
    }
    assert_eq!(row_value(&app, ConfigRow::Recap), "on");
    assert_eq!(row_value(&app, ConfigRow::RecapIdle), "10m");
}

/// A window of zero reads as off rather than as a zero-minute trigger, which
/// would look like an instant recap.
#[test]
fn test_recap_zero_reads_off() {
    let mut app = composition::app();
    {
        let mut snap = app.snapshot_or_stub();
        snap.recap_enabled = false;
        snap.recap_idle_secs = 0;
        app.sessions.current.capabilities.status.status_cache = Some(snap);
    }
    assert_eq!(row_value(&app, ConfigRow::Recap), "off");
    assert_eq!(row_value(&app, ConfigRow::RecapIdle), "off");
}
