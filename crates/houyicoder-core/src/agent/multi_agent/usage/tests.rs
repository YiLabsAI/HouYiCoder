//! Tests for the delegated-usage aggregator.

use houyicoder_context::{SessionEvent, SessionLogEntry};

use super::{SubagentUsage, aggregate_subagent_usage};
use houyicoder_context::EventId;
use houyicoder_context::SessionId;
use houyicoder_protocol::llm::{Usage, UsageProvenance};

fn entry(event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
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
        input_reported: true,
        cache_read_reported: true,
        cache_write_reported: true,
    }
}

/// A child whose endpoint reported the input total but not the cache-read
/// count, so its zero cache-read is unknown rather than a confirmed miss.
fn child_return_read_unreported(input: u64, output: u64) -> SessionEvent {
    SessionEvent::SubagentReturn {
        child_session_id: "child".into(),
        status: "completed".into(),
        summary: String::new(),
        result_ref: String::new(),
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
        input_reported: true,
        cache_read_reported: false,
        cache_write_reported: false,
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

#[test]
fn test_empty_delegation_neutral() {
    // Folding a childless delegation into a session's own usage must leave
    // it untouched; an all-unreported empty record is the AND-fold's zero and
    // would silently blank every childless session's cache rate in /status.
    let own = Usage {
        input_tokens: 1000,
        cache_read_input_tokens: 800,
        provenance: UsageProvenance {
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: false,
        },
        ..Default::default()
    };
    let empty = SubagentUsage::default().to_usage();
    assert_eq!(own.saturating_add(&empty), own);
    assert_eq!(
        own.saturating_add(&empty).cache_hit_rate(),
        Some(0.8),
        "a childless fold keeps the session's own cache rate"
    );
}

/// When a subagent return carries cache_write (>0), to_usage subtracts both
/// cache_read and cache_write from input so non_cached does not silently
/// absorb the write tokens. The assertion against a hard-coded total
/// (1000 = 500 + 200 + 300) avoids the constant-true trap.
#[test]
fn test_to_usage_subtracts_write() {
    let s = SubagentUsage {
        calls: 1,
        input_tokens: 1000,
        cache_read_input_tokens: 500,
        cache_write_input_tokens: 200,
        ..Default::default()
    };
    let u = s.to_usage();
    assert_eq!(u.input_tokens, 1000);
    assert_eq!(u.cache_read_input_tokens, 500);
    assert_eq!(u.cache_write_input_tokens, 200);
    // non_cached = 1000 - 500 - 200 = 300
    assert_eq!(u.non_cached_input_tokens, 300);
    assert_eq!(
        u.non_cached_input_tokens as u64
            + u.cache_read_input_tokens as u64
            + u.cache_write_input_tokens as u64,
        u.input_tokens as u64,
        "non_cached + cache_read + cache_write = input"
    );
}

/// A child whose endpoint did not report the cache-read count folds an unknown
/// into the parent's merged provenance, so the combined cache hit rate reads
/// unknown rather than a misleading 0% built from the child's zero cache-read.
#[test]
fn test_unreported_child_rate_unknown() {
    let events = vec![entry(child_return_read_unreported(100, 20))];
    let usage = aggregate_subagent_usage(&events).to_usage();
    assert!(
        usage.provenance.input_reported,
        "the child did report its input total"
    );
    assert!(
        !usage.provenance.cache_read_reported,
        "the unreported cache-read dilutes the merged provenance"
    );
    assert_eq!(
        usage.cache_hit_rate(),
        None,
        "an incomplete cache account reads unknown, not 0%"
    );
}

/// The complement: children that all report keep the merged rate computable,
/// so the AND-fold is not constant-unknown. One reported child yields its own
/// hit rate (80 of 100 input tokens came from the cache).
#[test]
fn test_reported_children_keep_rate() {
    let events = vec![entry(child_return(100, 20, 80))];
    let usage = aggregate_subagent_usage(&events).to_usage();
    assert!(
        usage.provenance.cache_account_complete(),
        "every child reported, so the merged account is complete"
    );
    assert_eq!(usage.cache_hit_rate(), Some(0.8));
}
