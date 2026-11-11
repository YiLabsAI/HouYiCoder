use std::thread;
use std::time::{Duration, Instant};

use super::UiWake;

/// A wake raised before the loop reaches its wait is not lost: the wait
/// returns at once and reports the producer's arrival.
#[test]
fn test_wake_before_wait_remembered() {
    let signal = UiWake::new();
    signal.wake();
    let start = Instant::now();
    assert!(signal.wait(Some(Duration::from_secs(5))));
    assert!(
        start.elapsed() < Duration::from_millis(100),
        "a pending wake returns without waiting"
    );
}

/// A producer's wake releases a waiter blocked with no timeout, which is the
/// shape an idle session uses.
#[test]
fn test_wake_releases_waiter() {
    let signal = UiWake::new();
    let waiting = signal.clone();
    let handle = thread::spawn(move || {
        let start = Instant::now();
        let woken = waiting.wait(None);
        (woken, start.elapsed())
    });
    thread::sleep(Duration::from_millis(20));
    signal.wake();
    let (woken, elapsed) = handle.join().expect("the waiter thread finishes");
    assert!(woken, "the wake is reported to the waiter");
    assert!(
        elapsed < Duration::from_secs(2),
        "the wake releases the waiter instead of leaving it blocked"
    );
}

/// A timeout with nothing pending reports no wake, so a timer expiry is never
/// mistaken for a producer arrival.
#[test]
fn test_wait_times_out_quietly() {
    let signal = UiWake::new();
    assert!(!signal.wait(Some(Duration::from_millis(10))));
}

/// Consuming a wake clears it, so one arrival cannot serve two turns of the
/// loop after the work it announced was already drained.
#[test]
fn test_wake_is_consumed_once() {
    let signal = UiWake::new();
    signal.wake();
    assert!(signal.wait(Some(Duration::from_millis(10))));
    assert!(!signal.wait(Some(Duration::from_millis(10))));
}
