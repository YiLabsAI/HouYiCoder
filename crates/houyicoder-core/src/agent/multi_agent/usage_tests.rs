//! Tests for the delegated-usage aggregator.

use houyicoder_context::{SessionEvent, SessionLogEntry};

use super::{SubagentUsage, aggregate_subagent_usage};

fn entry(event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: houyicoder_context::EventId::new(),
        session: houyicoder_context::SessionId::new(),
        ts: 0,
        prev_hash: None,
        event,
    }
}

fn child_return(input: u64, output: u64, cache_read: u64) -> SessionEvent {
    SessionEvent::SubagentReturn {
        child_session_id: "child".into(),
        status: "completed".into(),
        summary: String::new(),
        result_ref: String::new(),
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    }
}

#[test]
fn test_no_children_is_empty() {
    let usage = aggregate_subagent_usage(&[]);
    assert_eq!(usage.calls, 0);
    assert_eq!(usage.unmeasured_calls, 0);
}

#[test]
fn test_sums_every_return() {
    let events = vec![
        entry(child_return(100, 20, 80)),
        entry(child_return(50, 5, 0)),
    ];
    let usage = aggregate_subagent_usage(&events);
    assert_eq!(usage.calls, 2);
    assert_eq!(usage.input_tokens, 150);
    assert_eq!(usage.output_tokens, 25);
    assert_eq!(usage.cache_read_input_tokens, 80);
    assert_eq!(usage.unmeasured_calls, 0);
}

#[test]
fn test_unmeasured_child_counts() {
    let events = vec![entry(child_return(0, 0, 0))];
    let usage = aggregate_subagent_usage(&events);
    assert_eq!(usage.calls, 1);
    assert_eq!(usage.unmeasured_calls, 1);
}

#[test]
fn test_partial_measurement_kept() {
    // A child that reported only output is measured; the count must not claim
    // its cost is unknown.
    let events = vec![entry(child_return(0, 7, 0))];
    let usage = aggregate_subagent_usage(&events);
    assert_eq!(usage.calls, 1);
    assert_eq!(usage.unmeasured_calls, 0);
}

#[test]
fn test_other_events_are_ignored() {
    let events = vec![entry(SessionEvent::UserInput {
        text: "hello".into(),
    })];
    let usage = aggregate_subagent_usage(&events);
    assert_eq!(usage, SubagentUsage::default());
}

#[test]
fn test_to_usage_maps_totals() {
    let events = vec![entry(child_return(100, 20, 80))];
    let usage = aggregate_subagent_usage(&events).to_usage();
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 20);
    assert_eq!(usage.total_tokens, 120);
    assert_eq!(usage.cache_read_input_tokens, 80);
    // Non-cached input is what was processed rather than reused.
    assert_eq!(usage.non_cached_input_tokens, 20);
}
