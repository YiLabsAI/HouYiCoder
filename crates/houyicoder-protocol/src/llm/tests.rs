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

#[test]
fn test_usage_rate_unknown_unreported() {
    // A usage whose cache-read count the endpoint did not report yields an
    // unknown rate, not a 0% that reads like a confirmed cache miss. The
    // cached value stays 0, but provenance says it was never reported.
    let u = Usage {
        input_tokens: 1000,
        cache_read_input_tokens: 0,
        provenance: UsageProvenance {
            input_reported: true,
            cache_read_reported: false,
            cache_write_reported: false,
        },
        ..Default::default()
    };
    assert_eq!(u.cache_hit_rate(), None);
    assert_eq!(u.cache_hit_pct(), None);
}

#[test]
fn test_provenance_rate_partial_account() {
    // One part of the account reported and the other not: the share is unknown,
    // even though the reported part is above zero. A quotient over the partial
    // sum would read higher than the truth, since the input it divides is short
    // by everything the endpoint left out.
    let cache_read_reported = UsageProvenance {
        input_reported: false,
        cache_read_reported: true,
        cache_write_reported: false,
    };
    assert_eq!(cache_read_reported.cache_hit_rate(500, 1_000), None);
    assert_eq!(cache_read_reported.cache_hit_pct(500, 1_000), None);

    let input_reported = UsageProvenance {
        input_reported: true,
        cache_read_reported: false,
        cache_write_reported: false,
    };
    assert_eq!(input_reported.cache_hit_rate(500, 1_000), None);

    // A complete account with no input to divide by has no share either.
    assert_eq!(UsageProvenance::ALL_REPORTED.cache_hit_rate(0, 0), None);
    assert_eq!(
        UsageProvenance::ALL_REPORTED.cache_hit_pct(500, 1_000),
        Some(50.0)
    );
}

#[test]
fn test_usage_rate_when_complete() {
    // Input and cache-read both reported: the rate is the share, clamped.
    let u = Usage {
        input_tokens: 1000,
        cache_read_input_tokens: 980,
        provenance: UsageProvenance {
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: false,
        },
        ..Default::default()
    };
    assert_eq!(u.cache_hit_rate(), Some(0.98));
    assert_eq!(u.cache_hit_pct(), Some(98.0));
}

#[test]
fn test_usage_rate_clamps_overshoot() {
    // A provider whose reported cache-read exceeds input cannot push the rate
    // above one; the clamp holds even with a complete account.
    let u = Usage {
        input_tokens: 100,
        cache_read_input_tokens: 150,
        provenance: UsageProvenance {
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: false,
        },
        ..Default::default()
    };
    assert_eq!(u.cache_hit_rate(), Some(1.0));
}

#[test]
fn test_usage_legacy_defaults_unreported() {
    // A record serialized before the provenance field existed carries no
    // provenance key. It deserializes to all-unreported, so an old log entry
    // is excluded from a steady-state cohort rather than read as a 0% sample.
    let legacy = r#"{"input_tokens":1000,"output_tokens":50,"total_tokens":1050,"non_cached_input_tokens":1000,"cache_read_input_tokens":0,"cache_write_input_tokens":0,"reasoning_tokens":0}"#;
    let u: Usage = serde_json::from_str(legacy).unwrap();
    assert_eq!(
        u.provenance,
        UsageProvenance::default(),
        "a legacy record with no provenance key defaults to all-unreported"
    );
    assert_eq!(u.cache_hit_rate(), None);
}

#[test]
fn test_usage_provenance_roundtrips() {
    // A complete provenance survives a serialize-deserialize cycle so a
    // freshly persisted record keeps its account-completeness through replay.
    let u = Usage {
        input_tokens: 1000,
        cache_read_input_tokens: 900,
        provenance: UsageProvenance {
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: true,
        },
        ..Default::default()
    };
    let json = serde_json::to_string(&u).unwrap();
    let back: Usage = serde_json::from_str(&json).unwrap();
    assert_eq!(back, u);
    assert_eq!(back.cache_hit_rate(), Some(0.9));
}

#[test]
fn test_usage_merge_conservative() {
    // A combined record is complete only when every summand reported the
    // field: an unreported part hides inside a total that otherwise reads
    // complete. saturating_add merges by AND.
    let complete = UsageProvenance {
        input_reported: true,
        cache_read_reported: true,
        cache_write_reported: false,
    };
    let unreported = UsageProvenance::default();
    assert!(!complete.merge(unreported).cache_account_complete());
    assert!(complete.merge(complete).cache_account_complete());

    let a = Usage {
        input_tokens: 500,
        cache_read_input_tokens: 400,
        provenance: complete,
        ..Default::default()
    };
    let b = Usage {
        input_tokens: 500,
        cache_read_input_tokens: 400,
        provenance: unreported,
        ..Default::default()
    };
    let sum = a.saturating_add(&b);
    assert_eq!(sum.input_tokens, 1000);
    assert_eq!(sum.cache_read_input_tokens, 800);
    assert!(
        !sum.provenance.cache_account_complete(),
        "a sum with an unreported part is not a complete account"
    );
    assert_eq!(sum.cache_hit_rate(), None);
}

#[test]
fn test_usage_identity_is_neutral() {
    // The identity is the seed an accumulator starts from: folding it into a
    // real record must leave that record's totals and completeness untouched,
    // so a single complete turn still reads complete instead of being diluted
    // by the all-unreported default.
    let complete = Usage {
        input_tokens: 1000,
        cache_read_input_tokens: 800,
        provenance: UsageProvenance {
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: false,
        },
        ..Default::default()
    };
    let id = Usage::identity();
    assert_eq!(complete.saturating_add(&id), complete);
    assert_eq!(id.saturating_add(&complete), complete);
    assert_eq!(id.cache_hit_rate(), None, "zero-input identity has no rate");
}
