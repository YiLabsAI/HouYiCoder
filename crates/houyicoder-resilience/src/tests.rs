//! Retry and backoff: attempt counts, the jitter window, and the breaker
//! handoff.

use super::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::sleep;

#[tokio::test]
async fn test_retry_succeeds_after_failures() {
    let cfg = Retry {
        max_attempts: 4,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(4),
    };
    let count = AtomicU32::new(0);
    let result: Result<u32, RetryError<&'static str>> = cfg
        .run(|_attempt| {
            let n = count.fetch_add(1, Ordering::SeqCst);
            async move { if n < 2 { Err("transient") } else { Ok(42) } }
        })
        .await;
    assert_eq!(result.unwrap(), 42);
    assert_eq!(count.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn test_retry_exhausts() {
    let cfg = Retry {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(4),
    };
    let result: Result<u32, RetryError<&'static str>> =
        cfg.run(|_| async { Err::<u32, _>("always") }).await;
    match result {
        Err(RetryError::Exhausted { attempts, last }) => {
            assert_eq!(attempts, 3);
            assert_eq!(last, "always");
        }
        _ => panic!("expected exhaustion"),
    }
}

#[tokio::test]
async fn test_retry_first_attempt_success() {
    let cfg = Retry::default();
    let result: Result<u32, RetryError<&'static str>> = cfg.run(|_| async { Ok(7) }).await;
    assert_eq!(result.unwrap(), 7);
}

#[tokio::test]
async fn test_short_circuits_non_retryable() {
    let cfg = Retry {
        max_attempts: 5,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(4),
    };
    let count = AtomicU32::new(0);
    let result: Result<u32, RetryError<&'static str>> = cfg
        .run_if(
            |_| {
                count.fetch_add(1, Ordering::SeqCst);
                async { Err::<u32, _>("fatal") }
            },
            |e| *e != "fatal",
        )
        .await;
    assert!(matches!(result, Err(RetryError::Fatal { error: "fatal" })));
    assert_eq!(count.load(Ordering::SeqCst), 1); // no retry on fatal.
}

#[tokio::test]
async fn test_run_if_retries_retryable() {
    let cfg = Retry {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(4),
    };
    let count = AtomicU32::new(0);
    let result: Result<u32, RetryError<&'static str>> = cfg
        .run_if(
            |_| {
                let n = count.fetch_add(1, Ordering::SeqCst);
                async move { if n == 0 { Err("transient") } else { Ok(9) } }
            },
            |_| true,
        )
        .await;
    assert_eq!(result.unwrap(), 9);
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

// --- CircuitBreaker: pure-logic state machine ---

fn inner_at(threshold: u32, cool_down: Duration) -> BreakerInner {
    BreakerInner::new(BreakerConfig {
        failure_threshold: threshold,
        cool_down,
    })
}

#[test]
fn test_inner_closed_allows() {
    let mut b = inner_at(3, Duration::from_secs(10));
    let now = Instant::now();
    assert_eq!(b.state, BreakerState::Closed);
    assert!(b.allow(now));
    assert!(b.allow(now));
}

#[test]
fn test_inner_opens_at_threshold() {
    let mut b = inner_at(3, Duration::from_secs(10));
    let now = Instant::now();
    b.record_failure(now);
    b.record_failure(now);
    assert_eq!(b.state, BreakerState::Closed);
    assert!(b.allow(now));
    b.record_failure(now);
    assert_eq!(b.state, BreakerState::Open);
    assert!(!b.allow(now));
}

#[test]
fn test_half_open_after_cooldown() {
    let mut b = inner_at(2, Duration::from_secs(10));
    let t0 = Instant::now();
    b.record_failure(t0);
    b.record_failure(t0);
    assert_eq!(b.state, BreakerState::Open);
    // before cool-down: rejected
    assert!(!b.allow(t0 + Duration::from_secs(5)));
    // after cool-down: transitions to HalfOpen and permits the probe
    assert!(b.allow(t0 + Duration::from_secs(11)));
    assert_eq!(b.state, BreakerState::HalfOpen);
}

#[test]
fn test_inner_half_open_success() {
    let mut b = inner_at(1, Duration::from_secs(1));
    let t0 = Instant::now();
    b.record_failure(t0); // trips Open at threshold 1
    assert_eq!(b.state, BreakerState::Open);
    assert!(b.allow(t0 + Duration::from_secs(2))); // HalfOpen
    b.record_success();
    assert_eq!(b.state, BreakerState::Closed);
    assert_eq!(b.consecutive_failures, 0);
}

#[test]
fn test_inner_half_open_failure() {
    let mut b = inner_at(1, Duration::from_secs(1));
    let t0 = Instant::now();
    b.record_failure(t0);
    assert!(b.allow(t0 + Duration::from_secs(2))); // HalfOpen
    let t1 = t0 + Duration::from_secs(3);
    b.record_failure(t1);
    assert_eq!(b.state, BreakerState::Open);
    assert_eq!(b.opened_at, Some(t1));
}

/// delay_for grows the backoff per attempt, capped at max_delay, jittered
/// into [base*2^(a-1)/2, base*2^(a-1)*3/2). A server-suggested delay
/// bypasses the backoff ceiling (a rate-limited account waits the server's
/// window, not the max-delay tick).
#[test]
fn test_backoff_grows_server_overrides() {
    let r = Retry {
        max_attempts: 4,
        base_delay: Duration::from_millis(100),
        max_delay: Duration::from_millis(800),
    };
    // attempt 1: backoff in [50, 150).
    let d1 = r.delay_for(1, None);
    assert!(d1 >= Duration::from_millis(50) && d1 < Duration::from_millis(150));
    // attempt 2: 2*base = 200 → jitter [100, 300).
    let d2 = r.delay_for(2, None);
    assert!(d2 >= Duration::from_millis(100) && d2 < Duration::from_millis(300));
    // attempt 4: capped at max_delay 800 → jitter [400, 1200).
    let d4 = r.delay_for(4, None);
    assert!(d4 >= Duration::from_millis(400) && d4 < Duration::from_millis(1200));
    // A server directive bypasses the ceiling (5s > 800ms max).
    assert_eq!(
        r.delay_for(1, Some(Duration::from_secs(5))),
        Duration::from_secs(5)
    );
}

#[test]
fn test_inner_success_resets_closed() {
    let mut b = inner_at(5, Duration::from_secs(10));
    let now = Instant::now();
    b.record_failure(now);
    b.record_failure(now);
    b.record_success();
    assert_eq!(b.consecutive_failures, 0);
    assert_eq!(b.state, BreakerState::Closed);
}

// --- CircuitBreaker: thread-safe wrapper ---

#[test]
fn test_breaker_open_then_half() {
    let cfg = BreakerConfig {
        failure_threshold: 3,
        cool_down: Duration::from_millis(20),
    };
    let b = CircuitBreaker::new(cfg);
    assert_eq!(b.state(), BreakerState::Closed);
    assert!(b.allow());
    b.record_failure();
    b.record_failure();
    assert_eq!(b.state(), BreakerState::Closed);
    b.record_failure();
    assert_eq!(b.state(), BreakerState::Open);
    assert!(!b.allow());
    // wait out the cool-down; next allow flips to HalfOpen
    sleep(Duration::from_millis(30));
    assert!(b.allow());
    assert_eq!(b.state(), BreakerState::HalfOpen);
}

#[test]
fn test_breaker_concurrent_failures_trip() {
    // Multiple threads hammering record_failure must still trip exactly
    // once into Open; state remains stable (no panic, no double-trip).
    let cfg = BreakerConfig {
        failure_threshold: 10,
        cool_down: Duration::from_secs(1),
    };
    let b = Arc::new(CircuitBreaker::new(cfg));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let b = b.clone();
        handles.push(spawn(move || {
            for _ in 0..5 {
                b.record_failure();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(b.state(), BreakerState::Open);
    assert!(!b.allow());
}

// --- Retry::run_guarded integration ---

#[tokio::test]
async fn test_breaker_short_circuits() {
    let retry = Retry {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(4),
    };
    let breaker = CircuitBreaker::new(BreakerConfig {
        failure_threshold: 1,
        cool_down: Duration::from_secs(60),
    });
    // trip the breaker first
    breaker.record_failure();
    assert_eq!(breaker.state(), BreakerState::Open);
    let count = AtomicU32::new(0);
    let res: Result<u32, GuardedError<&'static str>> = retry
        .run_guarded(
            |_| {
                count.fetch_add(1, Ordering::SeqCst);
                async { Ok::<u32, &'static str>(1) }
            },
            |_| true,
            &breaker,
        )
        .await;
    assert!(matches!(res, Err(GuardedError::BreakerOpen)));
    // operation never ran: breaker rejected before the first attempt
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_guarded_success_closes() {
    let retry = Retry {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(4),
    };
    let breaker = CircuitBreaker::new(BreakerConfig {
        failure_threshold: 2,
        cool_down: Duration::from_secs(60),
    });
    // pre-fail to near-trip so success path is exercised meaningfully
    breaker.record_failure();
    let res: Result<u32, GuardedError<&'static str>> = retry
        .run_guarded(|_| async { Ok::<u32, &'static str>(7) }, |_| true, &breaker)
        .await;
    assert_eq!(res.unwrap(), 7);
    assert_eq!(breaker.state(), BreakerState::Closed);
}

#[tokio::test]
async fn test_guarded_failure_recorded() {
    let retry = Retry {
        max_attempts: 2,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let breaker = CircuitBreaker::new(BreakerConfig {
        failure_threshold: 2,
        cool_down: Duration::from_secs(60),
    });
    let res: Result<u32, GuardedError<&'static str>> = retry
        .run_guarded(
            |_| async { Err::<u32, &'static str>("boom") },
            |_| true,
            &breaker,
        )
        .await;
    assert!(matches!(
        res,
        Err(GuardedError::Retry(RetryError::Exhausted { .. }))
    ));
    // two attempts -> two failures recorded -> trips Open
    assert_eq!(breaker.state(), BreakerState::Open);
}
