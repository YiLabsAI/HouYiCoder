//! Tests for the unix terminal boundary.

use std::fs::File;
use std::os::fd::AsFd;

use super::probe;
use super::probe_hangup;

/// A test process has no terminal of its own, so the answer here is no
/// hangup. What this pins is that the probe returns instead of blocking or
/// failing: a probe that panicked would end a session that is still fine.
#[test]
fn test_probe_answers_without_terminal() {
    assert!(
        !probe_hangup(),
        "a descriptor with no hangup is not reported as one"
    );
}

/// A quiet descriptor is not a hangup, and the answer comes from the
/// descriptor itself: the probe is what tells a session whose terminal
/// went away to end rather than spin.
#[test]
fn test_probe_reports_live_descriptor() {
    let quiet = File::open("/dev/null").expect("open a quiet descriptor");
    assert!(
        !probe(Some(quiet.as_fd())),
        "a quiet descriptor must not end the session"
    );
    assert!(
        !probe(None),
        "no descriptor to watch is not a hangup either"
    );
}
