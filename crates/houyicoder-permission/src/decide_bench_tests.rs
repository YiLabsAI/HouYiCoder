//! Hand-rolled decide latency bench. A full bench harness would pull a heavy
//! dep tree for one bench, so this measures p50 / p99 / p99.9 with
//! std::time::Instant instead. Runs via cargo test with the ignored flag and
//! nocapture; ignored by default so it never enters the regular check gate.

#![cfg(test)]

use std::time::Instant;

use crate::decision::Outcome;
use crate::gate::{DefaultModeGate, ModeGate};
use crate::mode::{PermissionMode, ToolRequest};

/// Run decide N times against a gate, return sorted latencies in nanoseconds
/// so the caller can read any percentile.
fn decide_latencies(
    gate: &DefaultModeGate,
    req: &ToolRequest,
    warmup: usize,
    n: usize,
) -> Vec<u64> {
    for _ in 0..warmup {
        std::hint::black_box(gate.decide(req));
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        let d = gate.decide(req);
        out.push(t.elapsed().as_nanos() as u64);
        std::hint::black_box(d);
    }
    out.sort_unstable();
    out
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn report(label: &str, sorted: &[u64]) {
    eprintln!(
        "  {label}: p50={}µs p99={}µs p99.9={}µs",
        percentile(sorted, 0.50) / 1000,
        percentile(sorted, 0.99) / 1000,
        percentile(sorted, 0.999) / 1000,
    );
}

/// A bench, not a correctness test: the gate decide must stay in the
/// microsecond range per call so it never shows up on the agent hot path.
/// The p99 threshold is generous (5ms) so it only catches a catastrophic
/// regression (a lock convoys, a regex blows up), not machine jitter. Runs
/// ignored so it stays out of the regular test gate; invoke with --ignored.
#[test]
#[ignore]
fn test_decide_p99_stays_microsecond() {
    let gate = DefaultModeGate::with_mode(PermissionMode::Auto);

    // A pure read under Auto: mode-default Allow.
    let read = ToolRequest {
        tool_name: "read",
        input: Some(&serde_json::json!({"path": "/tmp/x"})),
        is_destructive: false,
        is_read_only: true,
        native_requires_approval: false,
    };
    // A bash command the detection validator escalates (rm).
    let rm = ToolRequest {
        tool_name: "bash",
        input: Some(&serde_json::json!({"command": "rm -rf /tmp/x"})),
        is_destructive: true,
        is_read_only: false,
        native_requires_approval: true,
    };
    // A glob of a protected path: safety validator escalates.
    let glob = ToolRequest {
        tool_name: "glob",
        input: Some(&serde_json::json!({"pattern": ".git/config"})),
        is_destructive: false,
        is_read_only: true,
        native_requires_approval: false,
    };

    eprintln!("decide latency (10000 samples, 1000 warmup):");
    let read_lat = decide_latencies(&gate, &read, 1000, 10000);
    let rm_lat = decide_latencies(&gate, &rm, 1000, 10000);
    let glob_lat = decide_latencies(&gate, &glob, 1000, 10000);
    report("read  (Allow)", &read_lat);
    report("rm    (Ask)", &rm_lat);
    report("glob  (Ask)", &glob_lat);

    // Sanity: the three paths land the expected outcomes.
    assert_eq!(gate.decide(&read).outcome(), Outcome::Allow);
    assert_eq!(gate.decide(&rm).outcome(), Outcome::Ask);
    assert_eq!(gate.decide(&glob).outcome(), Outcome::Ask);

    // p99 must stay well under 5ms even on a slow CI box. The gate decide
    // is per-tool-call (not per-token), so a regression past this is a bug.
    let p99_us = percentile(&rm_lat, 0.99) / 1000;
    assert!(
        p99_us < 5000,
        "decide p99 regressed past 5ms: {p99_us}µs (rm case)"
    );
}
