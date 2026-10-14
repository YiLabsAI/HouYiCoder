use super::*;
use reqwest::header::HeaderMap;

fn headers(raw: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    if !raw.is_empty() {
        h.insert("retry-after", raw.parse().unwrap());
    }
    h
}

#[test]
fn test_retry_after_integer_seconds() {
    assert_eq!(parse_retry_after(&headers("30")), Some(30_000));
    assert_eq!(parse_retry_after(&headers("  5  ")), Some(5_000));
}

#[test]
fn test_absent_header_is_none() {
    let empty = HeaderMap::new();
    assert!(parse_retry_after(&empty).is_none());
    assert!(parse_retry_after(&headers("not-a-date-or-number")).is_none());
}

#[test]
fn test_past_date_yields_zero() {
    // A fixed past date parses + yields 0 (retry now, never negative).
    let past = "Sun, 06 Nov 1994 08:49:37 GMT";
    assert_eq!(parse_retry_after(&headers(past)), Some(0));
}

#[test]
fn test_future_date_parses() {
    // A far-future date yields a positive delay (the ms until then).
    let future = "Wed, 21 Oct 2099 07:28:00 GMT";
    let got = parse_retry_after(&headers(future)).expect("future date parses");
    assert!(
        got > 0,
        "a far-future date yields a positive wait, got {got}"
    );
}

#[test]
fn test_status_429_carries_delay() {
    match classify_status(429, Some(5_000)) {
        ProviderError::RateLimit { retry_after_ms } => {
            assert_eq!(retry_after_ms, Some(5_000));
        }
        other => panic!("429 -> RateLimit, got {other:?}"),
    }
}

#[test]
fn test_body_quota_vs_ratelimit() {
    // A 429 with a quota-exhaustion body is non-retryable QuotaExceeded;
    // a bare 429 is retryable RateLimit.
    assert!(matches!(
        classify_with_body(429, None, "quota exceeded"),
        ProviderError::QuotaExceeded
    ));
    assert!(matches!(
        classify_with_body(429, None, "slow down"),
        ProviderError::RateLimit {
            retry_after_ms: None
        }
    ));
}

#[test]
fn test_classify_body_context_overflow() {
    assert!(matches!(
        classify_with_body(400, None, "context length exceeded"),
        ProviderError::ContextOverflow {
            enforced_limit: None
        }
    ));
    assert!(matches!(
        classify_with_body(422, None, "maximum context window"),
        ProviderError::ContextOverflow {
            enforced_limit: None
        }
    ));
}

#[test]
fn test_extract_openai_style_limit() {
    // OpenAI: "This model's maximum context length is 200000 tokens".
    assert_eq!(
        extract_context_limit("This model's maximum context length is 200000 tokens"),
        Some(200_000)
    );
    // Commas as thousands separators.
    assert_eq!(
        extract_context_limit("maximum context length is 200,000 tokens"),
        Some(200_000)
    );
}

#[test]
fn test_cerebras_limit_anchor_wins() {
    // "current length is X while limit is Y" — Y is the real window.
    assert_eq!(
        extract_context_limit("Current length is 250000 while limit is 200000"),
        Some(200_000)
    );
}

#[test]
fn test_extract_context_window_phrasing() {
    assert_eq!(
        extract_context_limit("request exceeds the context window of 1048576 tokens"),
        Some(1_048_576)
    );
}

#[test]
fn test_semantic_gate_rejects_numbers() {
    // Body has a number but no context-window semantics — do not parse.
    assert_eq!(extract_context_limit("error 400 bad request 42"), None);
}

#[test]
fn test_extract_range_rejects_misparse() {
    // A 4-digit status code that slipped past the gate is rejected.
    assert_eq!(extract_context_limit("context length error 413"), None);
}

#[test]
fn test_extract_none_without_number() {
    assert_eq!(
        extract_context_limit("the context window was exceeded"),
        None
    );
}

#[test]
fn test_classify_carries_extracted_limit() {
    // The error variant carries the enforced limit so the agent loop can
    // record it for the model.
    match classify_with_body(
        400,
        None,
        "This model's maximum context length is 200000 tokens",
    ) {
        ProviderError::ContextOverflow {
            enforced_limit: Some(200_000),
        } => {}
        other => panic!("expected ContextOverflow with limit, got {other:?}"),
    }
}

/// A 404 whose body names the model is ModelNotFound, not
/// InvalidRequest — so the caller's error message points at the model
/// id / catalog, not at auth.
#[test]
fn test_classify_model_not_found() {
    let e = classify_with_body(404, None, "model qwen3.8-max not found");
    assert!(
        matches!(e, ProviderError::ModelNotFound(_)),
        "model-not-found body → ModelNotFound, got {e:?}"
    );
    // A 400 with "no such model" also classifies.
    let e = classify_with_body(400, None, "Error: no such model: gpt-5");
    assert!(
        matches!(e, ProviderError::ModelNotFound(_)),
        "no-such-model body → ModelNotFound, got {e:?}"
    );
}

/// A 404 whose body does NOT name the model stays InvalidRequest (a
/// generic bad URL, not a model problem).
#[test]
fn test_classify_generic_400_invalid() {
    let e = classify_with_body(400, None, "bad request: missing parameter");
    assert!(
        matches!(e, ProviderError::InvalidRequest(_)),
        "generic 400 → InvalidRequest, got {e:?}"
    );
}

/// A 401/403 is Auth (not ModelNotFound), so the message points at the
/// API key.
#[test]
fn test_classify_auth_not_model() {
    let e = classify_with_body(401, None, "invalid api key");
    assert!(matches!(e, ProviderError::Auth), "401 → Auth, got {e:?}");
}
