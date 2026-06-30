//! ResourceBreaker — aggregate resource circuit breaker for spawned
//! processes. Wraps CircuitBreaker: trips Open when aggregate spawned CPU
//! exceeds budget, in-flight procs exceed cap, or a single command exceeds
//! its per-cmd budget N consecutive times. When Open, try_acquire rejects
//! new spawns for the cool-down.
//!
//! Catches the orphan-process class: even if a single command runs to its
//! wall-timeout, the aggregate breaker trips when total spawned CPU across
//! all in-flight tools exceeds the budget, killing everything + refusing
//! new spawns for the cool-down.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{BreakerState, CircuitBreaker};

/// Trip reason for observability + TUI status.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::enum_variant_names,
    reason = "breaker state names follow the circuit-breaker convention"
)]
pub enum TripReason {
    /// Aggregate spawned CPU seconds exceeded the budget.
    AggregateCpuExceeded { used: u64, budget: u64 },
    /// In-flight spawned process count exceeded the cap.
    InFlightProcsExceeded { count: u32, cap: u32 },
    /// A single command exceeded its per-cmd budget N consecutive times.
    PerCmdBudgetExceeded { consecutive: u32, threshold: u32 },
}

impl std::fmt::Display for TripReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AggregateCpuExceeded { used, budget } => {
                write!(f, "AggregateCpuExceeded (used {used}s / budget {budget}s)")
            }
            Self::InFlightProcsExceeded { count, cap } => {
                write!(f, "InFlightProcsExceeded (count {count} / cap {cap})")
            }
            Self::PerCmdBudgetExceeded {
                consecutive,
                threshold,
            } => {
                write!(
                    f,
                    "PerCmdBudgetExceeded ({consecutive} consecutive / threshold {threshold})"
                )
            }
        }
    }
}

/// A spawn lifecycle event the breaker aggregates. The enum carries the
/// payload so a single record call replaces per-event method names — the
/// variant (not the method name) says what is starting or ending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnEvent {
    /// One or more spawned processes began; counts toward the in-flight cap.
    Start { proc_count: u32 },
    /// A spawned command finished with its CPU usage and budget outcome.
    End {
        /// CPU seconds this command's process tree consumed.
        cpu_secs: u64,
        /// How many in-flight procs to release (mirrors the Start proc_count).
        proc_count: u32,
        /// Whether the command exceeded its per-cmd budget (e.g. wall timeout).
        exceeded_budget: bool,
    },
}

/// Config for the aggregate resource breaker. Defaults are industrial-grade:
/// 180s aggregate CPU (across all in-flight spawned tools), 200 in-flight
/// procs (fork-bomb backstop at the aggregate level), 3 consecutive per-cmd
/// budget exceed (consecutive-failure breaker pattern), 30s cool-down.
#[derive(Debug, Clone)]
pub struct ResourceBreakerConfig {
    /// Max total CPU seconds across all in-flight spawned commands.
    pub aggregate_cpu_budget_secs: u64,
    /// Max concurrent spawned processes (aggregate fork-bomb backstop).
    pub in_flight_proc_cap: u32,
    /// Consecutive per-cmd budget exceed before tripping.
    pub per_cmd_fail_threshold: u32,
    /// Cool-down before HalfOpen (delegates to CircuitBreaker).
    pub cool_down: Duration,
}

impl Default for ResourceBreakerConfig {
    fn default() -> Self {
        Self {
            aggregate_cpu_budget_secs: 180,
            in_flight_proc_cap: 200,
            per_cmd_fail_threshold: 3,
            cool_down: Duration::from_secs(30),
        }
    }
}

/// Inner mutable state guarded by the breaker's Mutex.
struct Inner {
    aggregate_cpu_secs: u64,
    in_flight_procs: u32,
    consecutive_cmd_fails: u32,
    last_trip_reason: Option<TripReason>,
}

impl Inner {
    fn new() -> Self {
        Self {
            aggregate_cpu_secs: 0,
            in_flight_procs: 0,
            consecutive_cmd_fails: 0,
            last_trip_reason: None,
        }
    }
}

/// The error returned when the breaker is Open and a new spawn is refused.
#[derive(Debug)]
pub struct ResourceBreakerOpen;

impl std::fmt::Display for ResourceBreakerOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "resource breaker open; spawn refused (cool-down)")
    }
}

impl std::error::Error for ResourceBreakerOpen {}

/// Aggregate resource circuit breaker. Share across tasks via
/// Arc<ResourceBreaker>. Wraps CircuitBreaker for the Open/Closed/HalfOpen
/// state machine; track aggregate CPU + in-flight procs + consecutive fails
/// in a separate inner Mutex.
pub struct ResourceBreaker {
    breaker: CircuitBreaker,
    config: ResourceBreakerConfig,
    inner: Mutex<Inner>,
}

impl ResourceBreaker {
    /// Create with config. The inner CircuitBreaker uses failure_threshold=1
    /// so aggregate-CPU and in-flight-proc exceed trip immediately on a single
    /// record_failure. The per-cmd N-consecutive logic is handled in Inner
    /// (only calls record_failure on the Nth consecutive fail), not by the
    /// breaker's failure_threshold.
    pub fn new(config: ResourceBreakerConfig) -> Self {
        let breaker = CircuitBreaker::new(crate::BreakerConfig {
            failure_threshold: 1,
            cool_down: config.cool_down,
        });
        Self {
            breaker,
            config,
            inner: Mutex::new(Inner::new()),
        }
    }

    /// Check whether a new spawn is allowed. Returns Err if the breaker is
    /// Open (cool-down active). Call before spawning.
    pub fn try_acquire(&self) -> Result<(), ResourceBreakerOpen> {
        if self.breaker.allow() {
            Ok(())
        } else {
            Err(ResourceBreakerOpen)
        }
    }

    /// Record a spawn lifecycle event. Start increments in-flight procs
    /// (trips immediately if the cap is exceeded). End releases in-flight,
    /// adds CPU (trips if the aggregate budget is exceeded), and tracks
    /// consecutive per-cmd budget exceedance (trips after N). A clean End
    /// records success, resetting the breaker. Call try_acquire before
    /// spawning so an Open breaker refuses the spawn up front.
    pub fn record(&self, event: SpawnEvent) {
        let mut inner = self.inner.lock().expect("resource breaker mutex");
        match event {
            SpawnEvent::Start { proc_count } => {
                inner.in_flight_procs = inner.in_flight_procs.saturating_add(proc_count);
                if inner.in_flight_procs > self.config.in_flight_proc_cap {
                    inner.last_trip_reason = Some(TripReason::InFlightProcsExceeded {
                        count: inner.in_flight_procs,
                        cap: self.config.in_flight_proc_cap,
                    });
                    self.breaker.record_failure();
                }
            }
            SpawnEvent::End {
                cpu_secs,
                proc_count,
                exceeded_budget,
            } => {
                inner.in_flight_procs = inner.in_flight_procs.saturating_sub(proc_count);
                inner.aggregate_cpu_secs = inner.aggregate_cpu_secs.saturating_add(cpu_secs);
                if exceeded_budget {
                    inner.consecutive_cmd_fails = inner.consecutive_cmd_fails.saturating_add(1);
                    if inner.consecutive_cmd_fails >= self.config.per_cmd_fail_threshold {
                        inner.last_trip_reason = Some(TripReason::PerCmdBudgetExceeded {
                            consecutive: inner.consecutive_cmd_fails,
                            threshold: self.config.per_cmd_fail_threshold,
                        });
                        self.breaker.record_failure();
                    }
                } else {
                    inner.consecutive_cmd_fails = 0;
                    self.breaker.record_success();
                }
                if inner.aggregate_cpu_secs > self.config.aggregate_cpu_budget_secs {
                    inner.last_trip_reason = Some(TripReason::AggregateCpuExceeded {
                        used: inner.aggregate_cpu_secs,
                        budget: self.config.aggregate_cpu_budget_secs,
                    });
                    self.breaker.record_failure();
                }
            }
        }
    }

    /// Current breaker state (Closed / Open / HalfOpen).
    pub fn state(&self) -> BreakerState {
        self.breaker.state()
    }

    /// The last trip reason (for TUI status + event emission). None if never
    /// tripped or has recovered.
    pub fn trip_reason(&self) -> Option<TripReason> {
        self.inner
            .lock()
            .expect("resource breaker mutex")
            .last_trip_reason
            .clone()
    }

    /// Remaining cool-down when Open; None when Closed / HalfOpen / past the
    /// cool-down. Delegates to the inner breaker; read-only (does not probe).
    pub fn cool_down_remaining(&self) -> Option<Duration> {
        self.breaker.cool_down_remaining()
    }

    /// Wrap in Arc for sharing across tasks.
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default_industrial() {
        let c = ResourceBreakerConfig::default();
        assert_eq!(c.aggregate_cpu_budget_secs, 180);
        assert_eq!(c.in_flight_proc_cap, 200);
        assert_eq!(c.per_cmd_fail_threshold, 3);
        assert_eq!(c.cool_down, Duration::from_secs(30));
    }

    #[test]
    fn test_clean_cmds_dont_trip() {
        let b = ResourceBreaker::new(ResourceBreakerConfig::default());
        for _ in 0..10 {
            b.record(SpawnEvent::Start { proc_count: 1 });
            b.record(SpawnEvent::End {
                cpu_secs: 5,
                proc_count: 1,
                exceeded_budget: false,
            }); // 5s CPU each, clean
        }
        assert_eq!(b.state(), BreakerState::Closed);
        assert!(b.trip_reason().is_none());
    }

    #[test]
    fn test_aggregate_cpu_trips() {
        let b = ResourceBreaker::new(ResourceBreakerConfig {
            aggregate_cpu_budget_secs: 10,
            ..ResourceBreakerConfig::default()
        });
        b.record(SpawnEvent::Start { proc_count: 1 });
        b.record(SpawnEvent::End {
            cpu_secs: 11,
            proc_count: 1,
            exceeded_budget: false,
        }); // 11s > 10s budget
        assert_eq!(b.state(), BreakerState::Open);
        assert!(matches!(
            b.trip_reason(),
            Some(TripReason::AggregateCpuExceeded {
                used: 11,
                budget: 10
            })
        ));
    }

    #[test]
    fn test_active_procs_trip() {
        let b = ResourceBreaker::new(ResourceBreakerConfig {
            in_flight_proc_cap: 5,
            ..ResourceBreakerConfig::default()
        });
        b.record(SpawnEvent::Start { proc_count: 6 }); // 6 > 5
        assert_eq!(b.state(), BreakerState::Open);
        assert!(matches!(
            b.trip_reason(),
            Some(TripReason::InFlightProcsExceeded { count: 6, cap: 5 })
        ));
    }

    #[test]
    fn test_consecutive_per_cmd_fail() {
        let b = ResourceBreaker::new(ResourceBreakerConfig::default());
        b.record(SpawnEvent::Start { proc_count: 1 });
        b.record(SpawnEvent::End {
            cpu_secs: 1,
            proc_count: 1,
            exceeded_budget: true,
        }); // fail 1
        assert_eq!(b.state(), BreakerState::Closed);
        b.record(SpawnEvent::Start { proc_count: 1 });
        b.record(SpawnEvent::End {
            cpu_secs: 1,
            proc_count: 1,
            exceeded_budget: true,
        }); // fail 2
        assert_eq!(b.state(), BreakerState::Closed);
        b.record(SpawnEvent::Start { proc_count: 1 });
        b.record(SpawnEvent::End {
            cpu_secs: 1,
            proc_count: 1,
            exceeded_budget: true,
        }); // fail 3 -> trip
        assert_eq!(b.state(), BreakerState::Open);
        assert!(matches!(
            b.trip_reason(),
            Some(TripReason::PerCmdBudgetExceeded {
                consecutive: 3,
                threshold: 3
            })
        ));
    }

    #[test]
    fn test_success_resets_consecutive() {
        let b = ResourceBreaker::new(ResourceBreakerConfig::default());
        b.record(SpawnEvent::Start { proc_count: 1 });
        b.record(SpawnEvent::End {
            cpu_secs: 1,
            proc_count: 1,
            exceeded_budget: true,
        }); // fail 1
        b.record(SpawnEvent::Start { proc_count: 1 });
        b.record(SpawnEvent::End {
            cpu_secs: 1,
            proc_count: 1,
            exceeded_budget: true,
        }); // fail 2
        b.record(SpawnEvent::Start { proc_count: 1 });
        b.record(SpawnEvent::End {
            cpu_secs: 1,
            proc_count: 1,
            exceeded_budget: false,
        }); // success -> reset
        b.record(SpawnEvent::Start { proc_count: 1 });
        b.record(SpawnEvent::End {
            cpu_secs: 1,
            proc_count: 1,
            exceeded_budget: true,
        }); // fail 1 (not 3)
        assert_eq!(b.state(), BreakerState::Closed);
    }

    #[test]
    fn test_open_rejects_acquire() {
        let b = ResourceBreaker::new(ResourceBreakerConfig {
            aggregate_cpu_budget_secs: 1,
            ..ResourceBreakerConfig::default()
        });
        b.record(SpawnEvent::Start { proc_count: 1 });
        b.record(SpawnEvent::End {
            cpu_secs: 2,
            proc_count: 1,
            exceeded_budget: false,
        }); // trip
        assert!(b.try_acquire().is_err());
    }
}
