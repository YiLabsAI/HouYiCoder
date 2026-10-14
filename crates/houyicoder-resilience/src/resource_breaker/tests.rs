//! The resource breaker: the threshold, the cool-down, and the half-open
//! probe.

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
