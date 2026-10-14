//! Tests for this module.

use super::*;
use crate::agent::reward_snapshot::{CostWaste, FailureHotspot, RedundantCluster};
use crate::observability::evolution::RedundancyKind;

fn snap_with_retry(retry: u32) -> RewardSnapshot {
    RewardSnapshot {
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
        retry_after_error: retry,
        recalled_keys: vec!["lesson_retry_after_fail".into()],
        cost: CostWaste {
            cumulative_input: 10_000,
            cumulative_output: 500,
            cumulative_cache_read: 8_000,
            cumulative_reasoning: 200,
            cache_hit_ratio: Some(0.8),
            api_duration_ms: 0,
        },
    }
}

fn empty_listing() -> Vec<MemorySummary> {
    Vec::new()
}

/// The reward lesson prompt carries the blind-retry count, the failure
/// hotspot, the redundant cluster, and the recalled key — so the dream
/// has concrete patterns to extract lessons from.
#[test]
fn test_prompt_carries_reward_data() {
    let snap = snap_with_retry(2);
    let prompt = build_reward_lesson_prompt("/mem", &empty_listing(), "", &snap);
    assert!(prompt.contains("Blind retries: 2"), "retry count: {prompt}");
    assert!(
        prompt.contains("bash: 3 failures"),
        "failure hotspot: {prompt}"
    );
    assert!(prompt.contains("grep: 2x"), "redundant cluster: {prompt}");
    assert!(
        prompt.contains("lesson_retry_after_fail"),
        "recalled key: {prompt}"
    );
}

/// The prompt has NO "if nothing changed, say so" blanket exit — the
/// reward dream must process each pattern.
#[test]
fn test_prompt_has_no_exit() {
    let snap = snap_with_retry(2);
    let prompt = build_reward_lesson_prompt("/mem", &empty_listing(), "", &snap);
    assert!(
        !prompt.contains("If nothing changed"),
        "no blanket exit: {prompt}"
    );
}

/// The prompt does NOT reference consolidation phases (Phase 1-4) —
/// those belong to the consolidation dream, not the reward dream.
#[test]
fn test_prompt_has_no_phases() {
    let snap = snap_with_retry(2);
    let prompt = build_reward_lesson_prompt("/mem", &empty_listing(), "", &snap);
    assert!(
        !prompt.contains("Phase 1") && !prompt.contains("Phase 2"),
        "no consolidation phases: {prompt}"
    );
}

/// The per-pattern actionable-difference filter is present — the dream
/// must name the existing lesson that covers a pattern before skipping.
#[test]
fn test_prompt_has_pattern_filter() {
    let snap = snap_with_retry(2);
    let prompt = build_reward_lesson_prompt("/mem", &empty_listing(), "", &snap);
    assert!(
        prompt.contains("no actionable difference"),
        "actionable-difference filter: {prompt}"
    );
    assert!(
        prompt.contains("per-pattern filter, not a blanket exit"),
        "filter is per-pattern: {prompt}"
    );
}

/// The prompt tells the dream NOT to consolidate, prune, or scope-flow —
/// those are the full consolidation dream's job.
#[test]
fn test_prompt_scopes_out_consolidation() {
    let snap = snap_with_retry(2);
    let prompt = build_reward_lesson_prompt("/mem", &empty_listing(), "", &snap);
    assert!(
        prompt.contains("Do NOT consolidate"),
        "no consolidation: {prompt}"
    );
    assert!(
        !prompt.contains("Phase 4") && !prompt.contains("promote_memory"),
        "no scope flow: {prompt}"
    );
}

/// An empty listing renders the "no memories" hint so the dream knows the
/// store is fresh and every lesson is new.
#[test]
fn test_prompt_with_empty_listing() {
    let snap = snap_with_retry(2);
    let prompt = build_reward_lesson_prompt("/mem", &empty_listing(), "", &snap);
    assert!(
        prompt.contains("no memories yet"),
        "empty listing hint: {prompt}"
    );
}

/// The prompt includes the memory root path so the dream knows where
/// save_memory writes.
#[test]
fn test_prompt_carries_memory_root() {
    let snap = snap_with_retry(2);
    let prompt = build_reward_lesson_prompt(
        "/data/.houyicoder/projects/x/memory",
        &empty_listing(),
        "",
        &snap,
    );
    assert!(
        prompt.contains("/data/.houyicoder/projects/x/memory"),
        "memory root: {prompt}"
    );
}

/// format_reward_data (used by the prompt) does NOT carry the "Act on
/// these in Phase 2" instruction — that is consolidation-specific.
#[test]
fn test_data_no_phase_instruction() {
    let snap = snap_with_retry(2);
    let data = format_reward_data(&snap);
    assert!(
        !data.contains("Phase 2"),
        "no Phase 2 in reward data: {data}"
    );
    assert!(
        data.contains("Blind retries: 2"),
        "retry count present: {data}"
    );
}

/// format_reward (consolidation path) DOES carry the Phase 2 instruction,
/// so the consolidation dream still gets it.
#[test]
fn test_format_keeps_phase_instruction() {
    let snap = snap_with_retry(2);
    let full = format_reward(&snap);
    assert!(
        full.contains("Phase 2"),
        "Phase 2 instruction in consolidation path: {full}"
    );
}
