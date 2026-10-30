//! The termination watch reports a signal once, and reports nothing while no
//! signal has arrived. The signal itself is raised in a separate process
//! because a test cannot deliver one to its own process without also
//! disturbing the harness.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{TerminationWatch, watch_termination};

/// A fresh watch reports no termination, so the run loop's first turn runs.
#[test]
fn test_idle_watch_reports_nothing() {
    let watch = TerminationWatch {
        requested: Arc::new(AtomicBool::new(false)),
    };
    assert!(!watch.requested());
}

/// The flag the handler raises is the flag the loop reads, so a raised signal
/// ends the run instead of being dropped between the two.
#[test]
fn test_raised_flag_is_observed() {
    let requested = Arc::new(AtomicBool::new(false));
    let watch = TerminationWatch {
        requested: Arc::clone(&requested),
    };
    assert!(!watch.requested(), "no signal yet");
    requested.store(true, Ordering::Relaxed);
    assert!(watch.requested(), "the handler's store reaches the loop");
}

/// The exit decision folds the user's quit and the signal together, so the
/// loop has one condition to test rather than two places that can disagree.
#[test]
fn test_exit_folds_causes() {
    let requested = Arc::new(AtomicBool::new(false));
    let watch = TerminationWatch {
        requested: Arc::clone(&requested),
    };
    assert!(!watch.should_exit(false), "neither cause is present");
    assert!(watch.should_exit(true), "the user asked to quit");
    requested.store(true, Ordering::Relaxed);
    assert!(watch.should_exit(false), "a signal ends the run");
    assert!(watch.should_exit(true), "both causes agree");
}

/// Installing the watch registers the handlers. Registration is
/// process-wide and cannot be undone, so a second watch shares the first
/// one's flag instead of installing a handler nobody reads: a signal raised
/// through either is visible to both. The flag's value is not asserted here
/// because another case in this binary may have raised it already.
#[test]
fn test_watch_is_shared() {
    let first = watch_termination().expect("register the termination handlers");
    let second = watch_termination().expect("share the registration");
    first.requested.store(true, Ordering::Relaxed);
    assert!(
        second.requested(),
        "both watches read the one flag the handlers write"
    );
}
