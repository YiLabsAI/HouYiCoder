//! The idle wake for pending completion notifications: when the server
//! reports endings the parent model has not read, an idle session wakes
//! itself on a debounce, bounded in a row, and the user's own turn always
//! outranks the wake.

use super::super::*;
use crate::agent_message::{ServerEvent, SessionMessage};
use crate::records::TranscriptLine;
use crate::test_harness::live_app;
use crate::ui::switch_state::{MAX_AUTO_WAKE_TURNS, WAKE_BUDGET_COOLDOWN};
use houyicoder_protocol::envelope::RequestId;
use std::time::{Duration, Instant};

/// Deliver a pending-notification report the way the driver would.
fn report_pending(app: &mut App, count: u32) {
    app.handle_agent_message(SessionMessage::Event(ServerEvent::NotificationsPending {
        count,
    }));
}

/// Age the open window past the debounce deadline without sleeping: the
/// state records both when the window opened and the count it opened on, so
/// a test moves the instant back after one idle pass opened the window.
fn age_past_debounce(app: &mut App) {
    app.idle_drain(None, &mut false);
    app.ui.notification_wake.window_started = Some(
        Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("a second before now exists"),
    );
}

/// An idle session with no pending notifications neither opens a window nor
/// sends anything: the wake is a consumer, and nothing to consume is not a
/// wake condition.
#[test]
fn test_wake_sends_nothing() {
    let mut app = live_app();
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(
        !app.sessions.current.run.is_active(),
        "no pending count means no wake turn"
    );
    assert!(app.ui.notification_wake.pending_at_window.is_none());
}

/// The first report opens the debounce window rather than waking at once:
/// a second ending arriving a moment later must batch into the same turn.
#[test]
fn test_wake_debounces() {
    let mut app = live_app();
    app.sessions.current.last_run_final = true;
    report_pending(&mut app, 1);
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(
        !app.sessions.current.run.is_active(),
        "the first report must not wake within the debounce window"
    );
    assert_eq!(app.ui.notification_wake.pending_at_window, Some(1));
}

/// A count that changes mid-window restarts the window on the new figure,
/// so the wake consumes the batch rather than a snapshot the next ending
/// invalidated.
#[test]
fn test_wake_restarts_window() {
    let mut app = live_app();
    app.sessions.current.last_run_final = true;
    report_pending(&mut app, 1);
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    // The count grows before the window completes.
    report_pending(&mut app, 2);
    app.idle_drain(None, &mut dirty);
    assert_eq!(
        app.ui.notification_wake.pending_at_window,
        Some(2),
        "the window restarts on the settled figure"
    );
    assert!(
        !app.sessions.current.run.is_active(),
        "a moved count delays the wake to the new window"
    );
}

/// Once the window completes at idle, the wake sends the drain request as a
/// real turn: the run state opens, the pending count clears, and the
/// consecutive budget counts one.
#[test]
fn test_wake_sends_after_debounce() {
    let mut app = live_app();
    app.sessions.current.last_run_final = true;
    report_pending(&mut app, 1);
    age_past_debounce(&mut app);
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(
        app.sessions.current.run.is_active(),
        "the wake opens the run the drain request drives"
    );
    assert_eq!(
        app.sessions.current.notifications_pending, 0,
        "the sent request owns the count now; the next enqueue re-reports"
    );
    assert_eq!(app.ui.notification_wake.auto_wake_turns, 1);
    assert!(dirty, "the frame the wake lands on is dirty");
}

/// A busy session never wakes: the wake runs only at the same idle gate the
/// queued-message drain uses, so it cannot start a second turn under the
/// user's own running one.
#[test]
fn test_wake_waits_for_idle() {
    let mut app = live_app();
    report_pending(&mut app, 1);
    age_past_debounce(&mut app);
    app.sessions.current.run.start(RequestId(1), Instant::now());
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert_eq!(
        app.ui.notification_wake.auto_wake_turns, 0,
        "a busy session does not spend wake budget"
    );
}

/// A user submission resets the budget: after the user's own turn, the
/// session may wake again on its own.
#[test]
fn test_wake_user_resets_budget() {
    let mut app = live_app();
    app.ui.notification_wake.auto_wake_turns = MAX_AUTO_WAKE_TURNS;
    app.ui.notification_wake.user_interrupted();
    assert_eq!(app.ui.notification_wake.auto_wake_turns, 0);
}

/// After an interrupt the wake does not fire: the user's parked input goes
/// first, and the notification waits for the next clean run end. The wake
/// is gated on the same clean-end condition the queued-message drain uses.
#[test]
fn test_wake_skips_after_interrupt() {
    let mut app = live_app();
    app.sessions.current.last_run_final = false;
    report_pending(&mut app, 1);
    age_past_debounce(&mut app);
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(
        !app.sessions.current.run.is_active(),
        "the wake must not jump ahead of a parked input after an interrupt"
    );
    assert_eq!(app.ui.notification_wake.auto_wake_turns, 0);
}

/// An interrupted run parks the wake, but the debounce window must still
/// close. A window left open makes the loop's own wait return zero, so the
/// session spins on a wake it will never send.
#[test]
fn test_interrupt_closes_window() {
    let mut app = live_app();
    app.sessions.current.last_run_final = false;
    report_pending(&mut app, 1);
    age_past_debounce(&mut app);
    assert!(
        app.ui.notification_wake.window_started.is_some(),
        "precondition: an open window to close"
    );
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(
        app.ui.notification_wake.window_started.is_none(),
        "an interrupted run closes the window instead of spinning"
    );
    assert!(app.ui.notification_wake.pending_at_window.is_none());
    assert!(!app.sessions.current.run.is_active());
}

/// Sixteen consecutive wakes inside one burst is the ceiling: the next report
/// parks.
#[test]
fn test_wake_budget_stops() {
    let mut app = live_app();
    app.sessions.current.last_run_final = true;
    app.ui.notification_wake.auto_wake_turns = MAX_AUTO_WAKE_TURNS;
    report_pending(&mut app, 1);
    age_past_debounce(&mut app);
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(!app.sessions.current.run.is_active());
}

/// A session with no connection to enqueue through does not crash or spend
/// budget: the report parks and the next idle pass retries once a connection
/// exists.
#[test]
fn test_wake_survives_no_connection() {
    let mut app = live_app();
    app.sessions.current.connection = None;
    report_pending(&mut app, 1);
    age_past_debounce(&mut app);
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(!app.sessions.current.run.is_active());
    assert_eq!(app.ui.notification_wake.auto_wake_turns, 0);
}

/// Hitting the cap must close the debounce window and say so once. A window
/// left open makes the loop's own wait arithmetic return zero, so the session
/// spins at full CPU on a wake it will never send.
#[test]
fn test_wake_cap_closes_window() {
    let mut app = live_app();
    app.sessions.current.last_run_final = true;
    app.ui.notification_wake.auto_wake_turns = MAX_AUTO_WAKE_TURNS;
    report_pending(&mut app, 3);
    age_past_debounce(&mut app);
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(
        app.ui.notification_wake.window_started.is_none(),
        "the cap closes the window instead of leaving it open to spin"
    );
    assert!(app.ui.notification_wake.paused_notice_sent);
    let notices = app
        .sessions
        .current
        .transcript
        .lines()
        .iter()
        .filter(|line| matches!(line, TranscriptLine::System(s) if s.contains("waiting")))
        .count();
    assert_eq!(
        notices, 1,
        "the parked backlog is reported once, not per pass"
    );
    // A second pass reports nothing new.
    age_past_debounce(&mut app);
    app.idle_drain(None, &mut dirty);
    let again = app
        .sessions
        .current
        .transcript
        .lines()
        .iter()
        .filter(|line| matches!(line, TranscriptLine::System(s) if s.contains("waiting")))
        .count();
    assert_eq!(again, 1, "the notice is deduplicated across passes");
}

/// A quiet stretch longer than the cooldown resets the burst budget, so
/// endings spread through a long session never latch the wake shut.
#[test]
fn test_wake_cooldown_resets_budget() {
    let mut app = live_app();
    app.sessions.current.last_run_final = true;
    app.ui.notification_wake.auto_wake_turns = MAX_AUTO_WAKE_TURNS;
    app.ui.notification_wake.last_wake_at = Some(
        Instant::now()
            .checked_sub(WAKE_BUDGET_COOLDOWN + Duration::from_secs(1))
            .expect("past the cooldown"),
    );
    report_pending(&mut app, 1);
    age_past_debounce(&mut app);
    let mut dirty = false;
    app.idle_drain(None, &mut dirty);
    assert!(
        app.sessions.current.run.is_active(),
        "a wake-free stretch longer than the cooldown restores the budget"
    );
    assert_eq!(app.ui.notification_wake.auto_wake_turns, 1);
}
