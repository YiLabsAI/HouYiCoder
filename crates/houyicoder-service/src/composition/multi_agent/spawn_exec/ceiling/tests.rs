//! Tests for the liveness watchdog backoff.

use super::*;
use houyicoder_async::CancellationToken;
use houyicoder_core::agent::multi_agent::CutReason;
use houyicoder_core::agent::multi_agent::status_publisher::ChildLiveness;
use houyicoder_protocol::llm::Usage;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The probe interval doubles each silent probe, capped at MAX_PROBE.
#[test]
fn test_probe_interval_doubles() {
    let base = BASE_PROBE;
    assert_eq!(probe_interval(0), base);
    assert_eq!(probe_interval(1), (base * 2).min(MAX_PROBE));
    assert_eq!(probe_interval(2), (base * 4).min(MAX_PROBE));
    assert_eq!(probe_interval(3), (base * 8).min(MAX_PROBE));
    // Past the fourth probe the interval stays at the cap.
    assert_eq!(probe_interval(4), MAX_PROBE);
    assert_eq!(probe_interval(9), MAX_PROBE);
}

/// A drive that keeps producing heartbeats runs past the quiet window
/// without being cut, even though the window would end a silent drive. The
/// watchdog sees the heartbeat and resets its stall count each probe.
#[tokio::test]
async fn test_pulsing_drive_survives() {
    let liveness = Arc::new(ChildLiveness::new());
    let cancel = CancellationToken::new();
    let cut_reason: Arc<Mutex<Option<CutReason>>> = Arc::new(Mutex::new(None));
    let liv = liveness.clone();
    // A drive that sends heartbeats on the tool phase every half-probe and completes
    // after twice the quiet window.
    let window = TOOL_QUIET_WINDOW;
    let drive = async move {
        let started = std::time::Instant::now();
        loop {
            sleep(BASE_PROBE / 2).await;
            liv.record(LivenessPhase::Tool);
            if started.elapsed() >= window * 2 {
                return ("ok".to_string(), String::new(), Usage::default());
            }
        }
    };
    let (_s, _u, _usage) = watch_liveness(
        drive,
        liveness,
        Duration::from_millis(60),
        Duration::from_secs(30),
        cancel.clone(),
        cut_reason.clone(),
    )
    .await;
    assert!(
        cut_reason.lock().unwrap().is_none(),
        "a drive that keeps pulsing is not cut"
    );
}

/// A drive that produces no heartbeat and never ends on its own is cut once the
/// quiet window elapses, and the cut reason names the timeout.
#[tokio::test]
async fn test_dead_drive_is_cut() {
    let liveness = Arc::new(ChildLiveness::new());
    let cancel = CancellationToken::new();
    let cut_reason: Arc<Mutex<Option<CutReason>>> = Arc::new(Mutex::new(None));
    let cancel_for_drive = cancel.clone();
    // A drive that only ends when the watchdog cancels it.
    let drive = async move {
        cancel_for_drive.cancelled().await;
        ("cut".to_string(), String::new(), Usage::default())
    };
    let (_s, _u, _usage) = watch_liveness(
        drive,
        liveness,
        Duration::from_millis(60),
        Duration::from_secs(30),
        cancel,
        cut_reason.clone(),
    )
    .await;
    let reason = cut_reason.lock().unwrap().clone();
    assert!(
        matches!(reason, Some(CutReason::TimedOut { .. })),
        "a dead drive is cut with a timeout reason, got {reason:?}"
    );
}
