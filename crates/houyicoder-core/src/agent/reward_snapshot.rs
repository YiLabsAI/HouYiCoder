//! Typed reward snapshot for the reward-fed auto_dream loop.
//!
//! Built read-only at the FinalOutput injection point: lock OL + redundancy
//! briefly, clone out, drop locks, then hand the snapshot to
//! build_consolidation_prompt. observability stays read-only — no
//! cross-track write into the memory sidecar (the sidecar is keyed by memory
//! key; reward has no memory key, and the sidecar is advisory lossy, so a
//! cross-track write would break the observability read-only invariant).
//!
//! Data (RewardSnapshot) and rendering (format_reward) are separate so
//! prompt wording changes do not touch the projection layer (Type First).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::agent::obs_wire::SharedObservability;
use crate::agent::redundancy::RedundancyTracker;
use crate::observability::MetricsView;
use crate::observability::evolution::RedundancyKind;

/// Maximum tools surfaced by fail count, so a pathological tool storm does
/// not bloat the dream prompt.
const FAILURE_HOTSPOT_TOP: usize = 5;
/// Maximum distinct reasons kept per hotspot.
const FAILURE_REASONS_PER_HOTSPOT: usize = 3;
/// Maximum redundant clusters surfaced.
const REDUNDANT_CLUSTER_TOP: usize = 10;

/// One failure hotspot worth dreaming about.
#[derive(Debug, Clone, PartialEq)]
pub struct FailureHotspot {
    pub tool: String,
    pub fail_count: u32,
    pub top_reasons: Vec<String>,
}

/// One redundant-call cluster: same tool + same kind aggregated, so the
/// dream sees a repeated-call pattern as one row rather than many.
#[derive(Debug, Clone, PartialEq)]
pub struct RedundantCluster {
    pub tool: String,
    pub kind: RedundancyKind,
    pub count: u32,
}

/// Token-cost signals for the dream to mine. Low cache_hit_ratio = cache
/// thrash (compaction dropped useful prefix); cumulative tokens are the
/// session total so the dream can weigh this much spent vs this much
/// wasted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostWaste {
    pub cumulative_input: u64,
    pub cumulative_output: u64,
    pub cumulative_cache_read: u64,
    pub cumulative_reasoning: u64,
    pub cache_hit_ratio: Option<f64>,
    pub api_duration_ms: u64,
}

/// Typed read-only reward snapshot. Taken under short locks at the
/// FinalOutput seam, cloned out, then handed to the dream. The dream agent
/// sees this alongside the recall_stats + gate_violations it already had —
/// two reward channels (observability structured + memory gate-violations)
/// merged at the single prompt-assembly seam, each with one producer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RewardSnapshot {
    pub failures: Vec<FailureHotspot>,
    pub redundant: Vec<RedundantCluster>,
    /// Blind retries: same-input call re-issued after the prior same-input
    /// call failed, no intervening write. The reward signal (agent
    /// decision), distinct from fail_count (world state). The dream gate
    /// keys on this, not fail_count.
    pub retry_after_error: u32,
    /// Memory keys recalled this session (from MemoryRecall events in the
    /// trajectory). Lets the dream judge each recalled memory against the
    /// failures that followed — if a memory's claim is contradicted by what
    /// happened, fix or delete it. Closes the failure→falsify-lesson edge.
    pub recalled_keys: Vec<String>,
    pub cost: CostWaste,
}

/// Project a read-only reward snapshot. Locks OL + redundancy briefly to
/// clone out, drops both before returning. Never holds across an await —
/// the dream spawn is the caller's job after this returns.
pub fn project_reward_snapshot(
    obs: &SharedObservability,
    redundancy: &Mutex<RedundancyTracker>,
) -> RewardSnapshot {
    let mut snap = RewardSnapshot::default();
    if let Ok(ol) = obs.lock() {
        // Failures: top failing tools by fail_count.
        let mut ranked: Vec<_> = ol
            .tool_stats()
            .iter()
            .filter(|t| t.fail_count > 0)
            .collect();
        ranked.sort_by_key(|t| std::cmp::Reverse(t.fail_count));
        for t in ranked.into_iter().take(FAILURE_HOTSPOT_TOP) {
            snap.failures.push(FailureHotspot {
                tool: t.tool_name.clone(),
                fail_count: t.fail_count,
                top_reasons: t
                    .failure_reasons
                    .iter()
                    .take(FAILURE_REASONS_PER_HOTSPOT)
                    .cloned()
                    .collect(),
            });
        }
        // Cost waste: cumulative tokens from the last turn delta's
        // cumulative field (session total), cache ratio, api duration.
        if let Some(delta) = ol.last_turn_delta() {
            snap.cost.cumulative_input = delta.cumulative.input_tokens as u64;
            snap.cost.cumulative_output = delta.cumulative.output_tokens as u64;
            snap.cost.cumulative_cache_read = delta.cumulative.cache_read_input_tokens as u64;
            snap.cost.cumulative_reasoning = delta.cumulative.reasoning_tokens as u64;
            snap.cost.cache_hit_ratio = delta.cache_hit_ratio;
        }
        snap.cost.api_duration_ms = ol.cost().total_api_duration_ms;
        // Recalled keys: scan the trajectory's MemoryRecall events so the
        // dream can judge each recalled memory against the failures that
        // followed — the failure→falsify-lesson edge.
        for ev in ol.trajectory() {
            if let houyicoder_context::TurnEventKind::MemoryRecall { keys, .. } = &ev.kind {
                snap.recalled_keys.extend(keys.iter().cloned());
            }
        }
    }
    // Redundant: cluster records by (tool, kind) so the dream sees patterns,
    // not raw rows. kind is mapped to a label for the bucket key because
    // RedundancyKind is not Hash; the label maps back on emit.
    if let Ok(t) = redundancy.lock() {
        let mut bucket: HashMap<(String, &'static str), u32> = HashMap::new();
        for r in t.records().iter() {
            let label = match r.kind {
                RedundancyKind::SameBatch => "same_batch",
                RedundancyKind::CrossBatch => "cross_batch",
            };
            *bucket.entry((r.tool.clone(), label)).or_insert(0) += 1;
        }
        let mut clusters: Vec<((String, &'static str), u32)> = bucket.into_iter().collect();
        clusters.sort_by_key(|((_, _), c)| std::cmp::Reverse(*c));
        for ((tool, label), count) in clusters.into_iter().take(REDUNDANT_CLUSTER_TOP) {
            let kind = match label {
                "same_batch" => RedundancyKind::SameBatch,
                _ => RedundancyKind::CrossBatch,
            };
            snap.redundant.push(RedundantCluster { tool, kind, count });
        }
        snap.retry_after_error = t.retry_after_error();
    }
    snap
}

/// Render the reward snapshot's data (failures, redundant calls, blind
/// retries, recalled keys, cost) as a prompt section, without any
/// task-specific instruction. The consolidation prompt appends its own
/// "Act on these in Phase 2" instruction via format_reward; the reward
/// lesson prompt appends its own lesson-extraction instruction. Keeping the
/// data rendering separate from the instruction lets two prompt paths share
/// one projection without duplicating the rendering logic.
pub(crate) fn format_reward_data(snap: &RewardSnapshot) -> String {
    if snap.failures.is_empty() && snap.redundant.is_empty() && snap.cost.cumulative_input == 0 {
        return String::new();
    }
    let mut out = String::from("## Recent reward signal\n\n");
    if !snap.failures.is_empty() {
        out.push_str("Failures worth reflecting on (top tools by fail count):\n");
        for f in &snap.failures {
            let reasons = if f.top_reasons.is_empty() {
                String::from("(no captured reasons)")
            } else {
                f.top_reasons.join("; ")
            };
            out.push_str(&format!(
                "- {}: {} failures — {}\n",
                f.tool, f.fail_count, reasons
            ));
        }
        out.push('\n');
    }
    if !snap.redundant.is_empty() {
        out.push_str("Redundant calls (the model re-issued calls it should not have):\n");
        for c in &snap.redundant {
            let label = match c.kind {
                RedundancyKind::SameBatch => "same-message repeat",
                RedundancyKind::CrossBatch => "cross-turn context-loss re-read",
            };
            out.push_str(&format!("- {}: {}x ({})\n", c.tool, c.count, label));
        }
        out.push('\n');
    }
    if snap.retry_after_error > 0 {
        out.push_str(&format!(
            "Blind retries: {} same-input call(s) re-issued after the prior one failed, with no \
             intervening write. These are agent-decision signals (retrying a known-failed call \
             without changing anything), not world-state failures.\n",
            snap.retry_after_error
        ));
        out.push('\n');
    }
    if !snap.recalled_keys.is_empty() {
        out.push_str("Memories recalled this session:\n");
        for k in &snap.recalled_keys {
            out.push_str(&format!("- {k}\n"));
        }
        out.push_str(
            "\nFor each recalled memory, judge whether it caused or failed to prevent the \
             failures above. If a memory's claim is contradicted by what happened — for example \
             a recalled lesson that the agent followed yet still failed — fix or delete it.\n\n",
        );
    }
    if snap.cost.cumulative_input > 0 {
        let ratio = snap
            .cost
            .cache_hit_ratio
            .map(|r| format!("{:.0}%", r * 100.0))
            .unwrap_or_else(|| String::from("unknown"));
        out.push_str(&format!(
            "Token spend this session: {} in / {} out / {} reasoning (cache hit {})\n",
            snap.cost.cumulative_input,
            snap.cost.cumulative_output,
            snap.cost.cumulative_reasoning,
            ratio,
        ));
    }
    out
}

/// Render the reward snapshot as a prompt section for the consolidation
/// dream. Wraps format_reward_data with the "Act on these in Phase 2"
/// instruction that ties the reward signal into the consolidation prompt's
/// phase structure.
pub fn format_reward(snap: &RewardSnapshot) -> String {
    let data = format_reward_data(snap);
    if data.is_empty() {
        return String::new();
    }
    format!(
        "{data}\nAct on these in Phase 2: for each recurring failure pattern, call \
         save_memory with a lesson_ key capturing what to check or do \
         differently so the next session avoids it; for each redundant-call \
         pattern, capture the context-loss cause. A lesson with no actionable \
         difference from the existing index is not worth saving — skip it.\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::obs_wire::new_log;
    use crate::agent::redundancy::RedundancyTracker;
    use crate::observability::evolution::RedundancyKind;

    #[test]
    fn test_empty_session_projects_empty() {
        let obs = new_log(200_000);
        let redundancy = Mutex::new(RedundancyTracker::new());
        let snap = project_reward_snapshot(&obs, &redundancy);
        assert!(snap.failures.is_empty());
        assert!(snap.redundant.is_empty());
        assert_eq!(snap.cost.cumulative_input, 0);
        // Empty session renders nothing — the dream prompt stays tight.
        assert!(format_reward(&snap).is_empty());
    }

    #[test]
    fn test_snapshots_ol_failures_cost() {
        use crate::agent::obs_wire::{record_tool_outcomes, record_turn};
        use houyicoder_protocol::llm::Usage;
        let obs = new_log(200_000);
        let mut names = std::collections::HashMap::new();
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
        let snap = project_reward_snapshot(&obs, &redundancy);
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
}
