//! The direct shell command state: at most one command in flight, its own
//! request id settles it, and a cancel is visible before the reply lands.

use super::*;

fn request(n: u64) -> RequestId {
    RequestId(n)
}

#[test]
fn test_begin_marks_active() {
    let mut shell = ShellRunState::default();
    assert!(!shell.is_active(), "a new session runs no command");
    assert!(!shell.is_cancelling());
    shell.begin(request(7));
    assert!(shell.is_active());
    assert!(!shell.is_cancelling());
    assert_eq!(shell.request_id(), Some(request(7)));
}

#[test]
fn test_cancel_keeps_request() {
    let mut shell = ShellRunState::default();
    shell.begin(request(3));
    shell.begin_cancel();
    assert!(shell.is_cancelling(), "the stop is visible at once");
    assert_eq!(
        shell.request_id(),
        Some(request(3)),
        "the reply still answers the same request"
    );
}

#[test]
fn test_cancel_idle_ignored() {
    let mut shell = ShellRunState::default();
    shell.begin_cancel();
    assert!(
        !shell.is_active(),
        "a stop with no command running must not start one"
    );
}

#[test]
fn test_settle_requires_matching_request() {
    let mut shell = ShellRunState::default();
    shell.begin(request(5));
    assert!(!shell.settle(request(4)), "a stale reply settles nothing");
    assert!(shell.is_active(), "the running command survives");
    assert!(shell.settle(request(5)), "its own reply settles it");
    assert!(!shell.is_active());
}

#[test]
fn test_settle_after_cancel_clears() {
    let mut shell = ShellRunState::default();
    shell.begin(request(9));
    shell.begin_cancel();
    assert!(shell.settle(request(9)));
    assert!(!shell.is_active(), "the interrupted command stops running");
}
