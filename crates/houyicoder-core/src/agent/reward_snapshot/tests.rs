//! Tests for this module.

use super::*;
use crate::agent::obs_wire::new_log;
use crate::agent::obs_wire::{record_tool_outcomes, record_turn};
use crate::agent::redundancy::RedundancyTracker;
use crate::observability::evolution::RedundancyKind;
use houyicoder_protocol::llm::Usage;
use std::collections::HashMap;

#[test]
fn test_empty_session_captures_empty() {
    let obs = new_log(200_000);
    let redundancy = Mutex::new(RedundancyTracker::new());
    let snap = capture_reward_snapshot(&obs, &redundancy);
    assert!(snap.failures.is_empty());
    assert!(snap.redundant.is_empty());
    assert_eq!(snap.cost.cumulative_input, 0);
    // Empty session renders nothing — the dream prompt stays tight.
    assert!(format_reward(&snap).is_empty());
}

#[test]
fn test_snapshots_ol_failures_cost() {
    let obs = new_log(200_000);
    let mut names = HashMap::new();
    names.insert("c1".into(), "bash".into());
    let results = vec![("c1".into(), serde_json::json!({"error": "boom"}))];
    record_tool_outcomes(&obs, &results, &names);
    let u = Usage {
        input_tokens: 1000,
        output_tokens: 500,
        total_tokens: 1500,
        non_cached_input_tokens: 200,
        cache_read_input_tokens: 800,
        cache_write_input_tokens: 0,
        reasoning_tokens: 100,
    };
    record_turn(&obs, "model-a", &u, 30_000, 500, 200_000, 32_768);
    let redundancy = Mutex::new(RedundancyTracker::new());
    let snap = capture_reward_snapshot(&obs, &redundancy);
    assert_eq!(snap.failures.len(), 1);
    assert_eq!(snap.failures[0].tool, "bash");
    assert_eq!(snap.cost.cumulative_input, 1000);
    assert_eq!(snap.cost.cumulative_reasoning, 100);
    let out = format_reward(&snap);
    assert!(out.contains("bash: 1 failures"));
    assert!(out.contains("1000 in / 500 out"));
}

#[test]
fn test_format_reward_renders_signal() {
    let snap = RewardSnapshot {
        failures: vec![FailureHotspot {
            tool: "bash".into(),
            fail_count: 3,
            top_reasons: vec!["exit 1".into()],
        }],
        redundant: vec![RedundantCluster {
            tool: "grep".into(),
            kind: RedundancyKind::CrossBatch,
            count: 2,
        }],
        retry_after_error: 2,
        recalled_keys: vec!["build-gate".into()],
        cost: CostWaste {
            cumulative_input: 10_000,
            cumulative_output: 500,
            cumulative_cache_read: 8_000,
            cumulative_reasoning: 200,
            cache_hit_ratio: Some(0.8),
            api_duration_ms: 0,
        },
    };
    let out = format_reward(&snap);
    assert!(out.contains("bash: 3 failures"));
    assert!(out.contains("grep: 2x (cross-turn context-loss re-read)"));
    assert!(out.contains("10000 in / 500 out"));
    assert!(out.contains("cache hit 80%"));
    assert!(
        out.contains("Blind retries: 2"),
        "retry_after_error rendered: {out}"
    );
    assert!(
        out.contains("Memories recalled"),
        "recalled keys rendered: {out}"
    );
    assert!(out.contains("build-gate"), "recalled key rendered: {out}");
    assert!(
        out.contains("contradicted"),
        "falsify instruction rendered: {out}"
    );
}
