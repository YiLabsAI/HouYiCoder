//! The termination watch reports a signal once, and reports nothing while no
//! signal has arrived. The signal itself is raised in a separate process
//! because a test cannot deliver one to its own process without also
//! disturbing the harness.

use houyicoder_platform::desktop::terminal::TerminationWatch;

use super::watch_termination as install;

/// A fresh watch reports no termination, so the run loop's first turn runs.
#[test]
fn test_idle_watch_reports_nothing() {
    let watch = TerminationWatch::for_test(false);
    assert!(!watch.requested());
}

/// A raised flag is observed, so a delivered signal ends the run instead of
/// being dropped between the handler and the loop.
#[test]
fn test_raised_flag_is_observed() {
    let watch = TerminationWatch::for_test(true);
    assert!(watch.requested(), "the handler's store reaches the loop");
}

/// The exit decision folds the user's quit and the signal together, so the
/// loop has one condition to test rather than two places that can disagree.
#[test]
fn test_exit_folds_causes() {
    let watch = TerminationWatch::for_test(false);
    assert!(!watch.should_exit(false), "neither cause is present");
    assert!(watch.should_exit(true), "the user asked to quit");
    let raised = TerminationWatch::for_test(true);
    assert!(raised.should_exit(false), "a signal ends the run");
    assert!(raised.should_exit(true), "both causes agree");
}

/// Installing the watch registers the handlers. Registration is process-wide
/// and cannot be undone, so a second watch shares the first one's flag
/// instead of installing a handler nobody reads: a signal raised through
/// either is visible to both. The flag's value is not asserted here because
/// another case in this binary may have raised it already.
#[test]
fn test_watch_is_shared() {
    let first = install(wake_noop).expect("register the termination handlers");
    let second = install(wake_noop).expect("share the registration");
    drop(first);
    drop(second);
}

/// A wake that does nothing, for registrations made from tests.
fn wake_noop() {}
