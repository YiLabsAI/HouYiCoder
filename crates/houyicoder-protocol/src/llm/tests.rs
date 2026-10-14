//! Tests for this module.

use super::*;
use std::time::Duration;

#[test]
fn test_effort_serde_lowercase() {
    assert_eq!(serde_json::to_string(&EffortLevel::Low).unwrap(), "\"low\"");
    assert_eq!(
        serde_json::to_string(&EffortLevel::Medium).unwrap(),
        "\"medium\""
    );
    assert_eq!(
        serde_json::to_string(&EffortLevel::High).unwrap(),
        "\"high\""
    );
    assert_eq!(
        serde_json::from_str::<EffortLevel>("\"high\"").unwrap(),
        EffortLevel::High
    );
}

#[test]
fn test_effort_three_state_roundtrips() {
    // Each level survives a serialize -> deserialize cycle unchanged,
    // and an absent effort (Option::None) round-trips as null — the
    // durable-log + wire callers rely on the lowercase wire form.
    for level in [EffortLevel::Low, EffortLevel::Medium, EffortLevel::High] {
        let json = serde_json::to_string(&level).unwrap();
        let back: EffortLevel = serde_json::from_str(&json).unwrap();
        assert_eq!(back, level, "round-trip {level:?} via {json}");
    }
    let absent: Option<EffortLevel> = None;
    let json = serde_json::to_string(&absent).unwrap();
    assert_eq!(json, "null");
    let back: Option<EffortLevel> = serde_json::from_str(&json).unwrap();
    assert!(back.is_none(), "absent effort round-trips as None");
}

#[test]
fn test_usage_visible_output_saturates() {
    let u = Usage {
        output_tokens: 5,
        reasoning_tokens: 8,
        ..Default::default()
    };
    assert_eq!(u.visible_output_tokens(), 0); // saturates, no underflow.
}

#[test]
fn test_llm_event_serde_round() {
    let e = LlmEvent::TextDelta {
        id: "b1".into(),
        text: "hi".into(),
    };
    let json = serde_json::to_string(&e).unwrap();
    let back: LlmEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(back, e);
    assert!(json.contains("\"type\":\"TextDelta\""));
}

#[test]
fn test_rate_limit_carries_delay() {
    // Only RateLimit with a Retry-After carries a server delay; bare
    // RateLimit + other retryable errors carry none (backoff applies).
    assert_eq!(
        ProviderError::RateLimit {
            retry_after_ms: Some(5_000)
        }
        .retry_after_delay(),
        Some(Duration::from_millis(5_000))
    );
    assert_eq!(
        ProviderError::RateLimit {
            retry_after_ms: None
        }
        .retry_after_delay(),
        None
    );
    assert_eq!(ProviderError::Network.retry_after_delay(), None);
    assert_eq!(ProviderError::ProviderInternal.retry_after_delay(), None);
}
