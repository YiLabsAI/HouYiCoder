//! Reward-loop benchmark skeleton.

#![cfg(test)]

use houyicoder_context::{SessionId, TurnEvent, TurnEventKind};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct RoundMetrics {
    pub redundant_clusters: usize,
    pub tool_failures: u32,
    pub recovery_retries: u32,
    pub recall_keys: Vec<String>,
    pub net_tokens: u64,
}

fn collect_from_events(events: &[TurnEvent]) -> RoundMetrics {
    let mut m = RoundMetrics::default();
    let mut recall_keys: Vec<String> = Vec::new();
    for e in events {
        match &e.kind {
            TurnEventKind::ToolResult { output, .. } => {
                if output.get("error").is_some() {
                    m.tool_failures += 1;
                }
            }
            TurnEventKind::TurnUsage { recovery, .. } => {
                if *recovery {
                    m.recovery_retries += 1;
                }
            }
            TurnEventKind::MemoryRecall { keys, .. } => {
                recall_keys.extend_from_slice(keys);
            }
            _ => {}
        }
    }
    recall_keys.sort();
    recall_keys.dedup();
    m.recall_keys = recall_keys;
    m
}

fn passes_self_evolution(off: &RoundMetrics, on: &RoundMetrics) -> bool {
    let redundant_down = off.redundant_clusters as f64 * 0.8 >= on.redundant_clusters as f64;
    let retries_down = off.recovery_retries as f64 * 0.8 >= on.recovery_retries as f64;
    let recall_hit = !on.recall_keys.is_empty();
    let net_not_up = on.net_tokens <= off.net_tokens;
    redundant_down && retries_down && recall_hit && net_not_up
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{EventId, TurnEvent};

    fn mk(kind: TurnEventKind) -> TurnEvent {
        TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            kind,
        }
    }

    #[test]
    fn test_dry_run_structure() {
        let events = vec![
            mk(TurnEventKind::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({"error": "boom"}),
                duration_ms: 0,
            }),
            mk(TurnEventKind::TurnUsage {
                turn: 0,
                call_in_turn: 1,
                input_tokens: 100,
                output_tokens: 50,
                cache_read_input_tokens: 80,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
                model: "test".into(),
                recovery: true,
                effort: None,
            }),
            mk(TurnEventKind::MemoryRecall {
                text: "lesson".into(),
                keys: vec!["feedback_dedup_grep".into()],
                bytes: 6,
            }),
        ];
        let m = collect_from_events(&events);
        assert_eq!(m.tool_failures, 1);
        assert_eq!(m.recovery_retries, 1);
        assert_eq!(m.recall_keys, vec!["feedback_dedup_grep".to_string()]);
        let off = RoundMetrics {
            tool_failures: 5,
            recovery_retries: 5,
            redundant_clusters: 5,
            ..Default::default()
        };
        let on = RoundMetrics {
            tool_failures: 1,
            recovery_retries: 1,
            redundant_clusters: 1,
            recall_keys: vec!["feedback_dedup_grep".into()],
            ..Default::default()
        };
        assert!(passes_self_evolution(&off, &on));
    }

    #[test]
    #[ignore = "real dogfood run, needs real provider + snapshot API"]
    fn test_benchmark_eighteen_rounds() {}
}
