//! The liveness watchdog a delegation runs under.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use houyicoder_async::CancellationToken;
use houyicoder_core::agent::multi_agent::CutReason;
use houyicoder_core::agent::multi_agent::status_publisher::ChildLiveness;
use houyicoder_protocol::llm::Usage;
use tokio::time::sleep;

const CHECK_INTERVAL: Duration = Duration::from_secs(10);
pub(super) const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(180);
pub(super) const DEFAULT_MAX_WALL_CLOCK: Duration = Duration::from_secs(3600);

/// Await a child's drive under a liveness watchdog. A child that keeps
/// producing pulses may run past the old fixed deadline; the watchdog cuts
/// only when the child goes quiet for the idle window. An absolute cap still
/// bounds the total.
pub(super) async fn watch_liveness(
    drive: impl std::future::Future<Output = (String, String, Usage)>,
    liveness: Arc<ChildLiveness>,
    idle_timeout: Duration,
    max_wall_clock: Duration,
    cancel: CancellationToken,
    cut_reason: Arc<Mutex<Option<CutReason>>>,
) -> (String, String, Usage) {
    let mut drive = Box::pin(drive);
    let start = Instant::now();
    let check = CHECK_INTERVAL.min(idle_timeout);
    loop {
        tokio::select! {
            biased;
            done = &mut drive => return done,
            _ = cancel.cancelled() => return drive.await,
            _ = sleep(check) => {
                let elapsed = start.elapsed();
                let idle = elapsed_from_ms(liveness.last_pulse_ms());
                if idle >= idle_timeout {
                    record_cut(&cut_reason, CutReason::TimedOut {
                        secs: idle_timeout.as_secs(),
                        stage: Some(format!("{:?}", liveness.phase())),
                    });
                    cancel.cancel();
                    return drive.await;
                }
                if elapsed >= max_wall_clock {
                    record_cut(&cut_reason, CutReason::TimedOut {
                        secs: max_wall_clock.as_secs(),
                        stage: None,
                    });
                    cancel.cancel();
                    return drive.await;
                }
            }
        }
    }
}

fn record_cut(slot: &Arc<Mutex<Option<CutReason>>>, reason: CutReason) {
    if let Ok(mut s) = slot.lock() {
        *s = Some(reason);
    }
}

fn elapsed_from_ms(past_ms: u64) -> Duration {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(past_ms);
    Duration::from_millis(now_ms.saturating_sub(past_ms))
}
