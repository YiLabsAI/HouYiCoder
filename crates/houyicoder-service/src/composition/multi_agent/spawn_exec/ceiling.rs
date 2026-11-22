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

/// How long a tool may run with no I/O heartbeat before the watchdog cuts
/// it.
/// The backoff probes sum to this (the base plus three doublings), so a tool
/// that produced nothing across all four probes is cut at the last one.
#[cfg(not(test))]
const TOOL_QUIET_WINDOW: Duration = Duration::from_secs(225);
#[cfg(test)]
const TOOL_QUIET_WINDOW: Duration = Duration::from_millis(120);

pub(super) const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(180);
pub(crate) const DEFAULT_MAX_WALL_CLOCK: Duration = Duration::from_secs(3600);

/// Resolve a child run's active wall-clock ceiling so explicit timeout hints
/// can extend above the safety floor without shortening it.
pub(crate) fn effective_wall_clock(timeout_secs: Option<u64>, floor: Duration) -> Duration {
    timeout_secs
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .map(|d| d.max(floor))
        .unwrap_or(floor)
}

///Bounded grace window granted to a responsive child to emit a final plain-text
/// summary after a budget or quiet-window stop cancels its active step.
#[cfg(not(test))]
const WRAP_UP_WINDOW: Duration = Duration::from_secs(45);
#[cfg(test)]
const WRAP_UP_WINDOW: Duration = Duration::from_millis(80);

/// Directive injected into a child session when its active budget runs out so
/// it synthesizes verified findings and remaining gaps instead of vanishing.
pub(super) const WRAP_UP_DIRECTIVE: &str = "Your time budget has ended and tool calls are now disabled. Immediately write a concise final report covering: (1) verified findings and completed work so far, and (2) remaining unverified items.";

/// The probe interval at a given stall depth. The base doubles each probe
/// that finds no new heartbeat, capped at MAX_PROBE: 15s, 30s, 60s, 120s, 120s.
fn probe_interval(stalled: u32) -> Duration {
    let shift = stalled.min(3);
    (BASE_PROBE * (1u32 << shift)).min(MAX_PROBE)
}

/// Await a child's drive under a liveness watchdog. Time spent waiting on a
/// human approval is excluded from active wall-clock accounting; when an idle
/// or active wall-clock limit trips, an optional soft wrap-up trigger gets one
/// bounded window to synthesize partial findings before hard cancellation.
pub(super) async fn watch_liveness(
    drive: impl std::future::Future<Output = (String, String, Usage)>,
    liveness: Arc<ChildLiveness>,
    idle_timeout: Duration,
    max_wall_clock: Duration,
    cancel: CancellationToken,
    cut_reason: Arc<Mutex<Option<CutReason>>>,
    mut wrap_up: Option<Arc<dyn Fn() + Send + Sync>>,
) -> (String, String, Usage) {
    let mut drive = Box::pin(drive);
    let mut last_tick = Instant::now();
    let mut active_elapsed = Duration::ZERO;
    let mut wrap_deadline: Option<Instant> = None;
    let mut stalled_probes = 0u32;
    let mut last_heartbeat = liveness.last_heartbeat_ms();
    let mut last_phase = liveness.phase();
    loop {
        let phase = liveness.phase();
        if phase != last_phase {
            last_phase = phase;
            stalled_probes = 0;
        }
        let probe = if wrap_deadline.is_some() {
            BASE_PROBE
        } else if phase == LivenessPhase::Tool {
            probe_interval(stalled_probes)
        } else {
            BASE_PROBE
        }
        .min(max_wall_clock);
        tokio::select! {
            biased;
            done = &mut drive => return done,
            _ = cancel.cancelled() => return drive.await,
            _ = sleep(probe) => {
                let now = Instant::now();
                let tick_dt = now.saturating_duration_since(last_tick);
                last_tick = now;
                if let Some(deadline) = wrap_deadline {
                    if now >= deadline {
                        cancel.cancel();
                        return drive.await;
                    }
                    continue;
                }
                let phase = liveness.phase();
                if phase == LivenessPhase::Approval {
                    // Human decision latency is not agent compute and is
                    // bounded by the bus approval router itself.
                    last_heartbeat = liveness.last_heartbeat_ms();
                    continue;
                }
                active_elapsed = active_elapsed.saturating_add(tick_dt);
                let now_heartbeat = liveness.last_heartbeat_ms();
                if now_heartbeat != last_heartbeat {
                    stalled_probes = 0;
                    last_heartbeat = now_heartbeat;
                } else if phase == LivenessPhase::Tool {
                    stalled_probes = stalled_probes.saturating_add(1);
                }
                let idle = elapsed_from_ms(liveness.last_heartbeat_ms());
                let window = match phase {
                    LivenessPhase::Tool => TOOL_QUIET_WINDOW,
                    _ => idle_timeout,
                };
                let cut = if idle >= window {
                    Some(CutReason::TimedOut {
                        secs: window.as_secs(),
                        stage: Some(format!("{phase:?}")),
                    })
                } else if active_elapsed >= max_wall_clock {
                    Some(CutReason::TimedOut {
                        secs: max_wall_clock.as_secs(),
                        stage: None,
                    })
                } else {
                    None
                };
                if let Some(reason) = cut {
                    record_cut(&cut_reason, reason);
                    if let Some(trigger) = wrap_up.take() {
                        liveness.record(LivenessPhase::Streaming);
                        trigger();
                        wrap_deadline = Some(Instant::now() + WRAP_UP_WINDOW);
                        continue;
                    }
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
