//! Tests that a logged turn reads back as the usage the views show.

use super::recorded_turn_usage;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_protocol::llm::UsageProvenance;

/// A usage entry as the runner appends it: the counts, the model that ran
/// the call, and whether the endpoint reported each count.
fn turn_usage(model: &str) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: SessionEvent::TurnUsage {
            turn: 1,
            call_in_turn: 1,
            input_tokens: 12_000,
            output_tokens: 500,
            cache_read_input_tokens: 9_600,
            cache_write_input_tokens: 400,
            reasoning_tokens: 120,
            model: model.to_string(),
            recovery: false,
            effort: None,
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: false,
        },
    }
}

#[test]
fn test_recorded_turn_reads_back() {
    let entry = turn_usage("claude-sonnet-5-5");
    let (model, usage) = recorded_turn_usage(&entry.event).expect("a usage entry reads back");
    assert_eq!(model, "claude-sonnet-5-5");
    assert_eq!(usage.input_tokens, 12_000);
    assert_eq!(usage.total_tokens, 12_500);
    assert_eq!(usage.non_cached_input_tokens, 2_000);
    assert_eq!(usage.cache_hit_rate(), Some(0.8));
    // The flags travel with the counts: a record that did not report the
    // cache-write count reads back short of a full account.
    assert_eq!(
        usage.provenance,
        UsageProvenance {
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: false,
        }
    );
}

#[test]
fn test_other_events_read_none() {
    let cleared = SessionEvent::ContextCleared { prior_turn: 3 };
    assert!(recorded_turn_usage(&cleared).is_none());
}
