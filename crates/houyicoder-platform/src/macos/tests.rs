//! The bounded wait, driven against a real child.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::wait_bounded;

#[expect(
    clippy::disallowed_methods,
    reason = "the bounded wait is exercised against a real child"
)]
fn shell(script: &str) -> Child {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .stdout(Stdio::null())
        .spawn()
        .expect("a shell starts")
}

/// A child that outlives the deadline is reported as unfinished, and the
/// caller is left holding one it can kill.
#[test]
fn test_slow_child_times_out() {
    let mut child = shell("sleep 5");
    let started = Instant::now();
    let status = wait_bounded(&mut child, Duration::from_millis(20));
    assert!(status.is_none(), "the deadline bounds the wait");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the wait returned at the deadline rather than at the child"
    );
    drop(child.kill());
    drop(child.wait());
}

/// A child that exits before the deadline hands back its status, so a caller
/// can tell a failed run from a timed-out one.
#[test]
fn test_exited_child_reports() {
    let mut child = shell("exit 3");
    let status = wait_bounded(&mut child, Duration::from_secs(10)).expect("the child exits");
    assert_eq!(status.code(), Some(3), "the exit status comes back");
}
