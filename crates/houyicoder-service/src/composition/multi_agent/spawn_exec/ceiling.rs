//! The liveness watchdog a delegation runs under.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use houyicoder_api::agent_event::LivenessPhase;
use houyicoder_async::CancellationToken;
use houyicoder_core::agent::multi_agent::CutReason;
use houyicoder_core::agent::multi_agent::status_publisher::ChildLiveness;
use houyicoder_protocol::llm::Usage;
use tokio::time::sleep;

/// The first probe interval. A child that keeps producing output sees the
/// watchdog return at this cadence while it streams, then on an exponential
/// backoff while a long tool runs.
#[cfg(not(test))]
const BASE_PROBE: Duration = Duration::from_secs(15);
/// Shrunk so a time-based test runs in milliseconds rather than minutes.
#[cfg(test)]
const BASE_PROBE: Duration = Duration::from_millis(20);

/// The longest the backoff stretches a probe interval. A healthy long task
/// still gets checked this often, so a stall is found within one window of it
/// going quiet.
#[cfg(not(test))]
const MAX_PROBE: Duration = Duration::from_secs(120);
#[cfg(test)]
const MAX_PROBE: Duration = Duration::from_millis(80);

/// How long a tool may run with no I/O pulse before the watchdog cuts it.
/// The backoff probes sum to this (the base plus three doublings), so a tool
/// that produced nothing across all four probes is cut at the last one.
#[cfg(not(test))]
const TOOL_QUIET_WINDOW: Duration = Duration::from_secs(225);
#[cfg(test)]
const TOOL_QUIET_WINDOW: Duration = Duration::from_millis(120);

pub(super) const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(180);
pub(super) const DEFAULT_MAX_WALL_CLOCK: Duration = Duration::from_secs(3600);
/// How long a child may wait on a human answer before the watchdog ends
/// it. Matches the approval timeout the routing path enforces.
const APPROVAL_WINDOW: Duration = Duration::from_secs(300);

/// The probe interval at a given stall depth. The base doubles each probe
/// that finds no new pulse, capped at MAX_PROBE: 15s, 30s, 60s, 120s, 120s.
fn probe_interval(stalled: u32) -> Duration {
    let shift = stalled.min(3);
    (BASE_PROBE * (1u32 << shift)).min(MAX_PROBE)
}

/// Await a child's drive under a liveness watchdog. A child that keeps
/// producing pulses may run past the old fixed deadline; the watchdog cuts
/// only when the child goes quiet for the phase's window. The probe cadence
/// backs off while a long tool runs so a healthy build or test suite is not
/// polled at the streaming rate, and snaps back to the base the moment output
/// arrives. An absolute cap still bounds the total.
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
    let mut stalled_probes = 0u32;
    let mut last_pulse = liveness.last_pulse_ms();
    let mut last_phase = liveness.phase();
    loop {
        let phase = liveness.phase();
        if phase != last_phase {
            last_phase = phase;
            stalled_probes = 0;
        }
        // A long tool backs off; a streaming or approval wait stays agile so a
        // stall is caught at the streaming rate, not the tool rate.
        let probe = if phase == LivenessPhase::Tool {
            probe_interval(stalled_probes)
        } else {
            BASE_PROBE
        };
        let probe = probe.min(max_wall_clock);
        tokio::select! {
            biased;
            done = &mut drive => return done,
            _ = cancel.cancelled() => return drive.await,
            _ = sleep(probe) => {
                let elapsed = start.elapsed();
                let now_pulse = liveness.last_pulse_ms();
                if now_pulse != last_pulse {
                    // Output arrived since the last probe: reset the backoff.
                    stalled_probes = 0;
                    last_pulse = now_pulse;
                } else if phase == LivenessPhase::Tool {
                    stalled_probes = stalled_probes.saturating_add(1);
                }
                let idle = elapsed_from_ms(liveness.last_pulse_ms());
                let window = match phase {
                    LivenessPhase::Approval => APPROVAL_WINDOW,
                    LivenessPhase::Tool => TOOL_QUIET_WINDOW,
                    _ => idle_timeout,
                };
                if idle >= window {
                    record_cut(&cut_reason, CutReason::TimedOut {
                        secs: window.as_secs(),
                        stage: Some(format!("{phase:?}")),
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

#[cfg(test)]
mod tests;
