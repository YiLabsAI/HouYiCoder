//! HTTP-error classification shared by every provider camp. The two
//! chat-completion wire formats (OpenAI-compatible chat/completions and
//! Anthropic Messages) differ in request body, response shape, stream
//! events, and cache-hint lowering — but their HTTP transport layer is
//! identical: both run over reqwest, both honor Retry-After, both signal
//! quota/context-overflow/auth/rate-limit through the same HTTP status
//! codes + broadly-phrased error bodies. These helpers live here so a new
//! camp (Anthropic) reuses them instead of copy-pasting.
//!
//! Camp-specific — stays in each provider's module:
//! - request body construction (chat/completions vs Messages)
//! - response parsing (choices/message vs content blocks)
//! - stream-event parsing (delta chunks vs message_delta events)
//! - cache-hint lowering (prompt_cache_key vs cache_control blocks)
//! - usage field-name parsing (prompt_tokens vs input_tokens/cache_read)

use houyicoder_protocol::llm::ProviderError;

/// Classify an HTTP status into a ProviderError. Status codes are universal
/// across camps: 429 rate-limit, 413/414 context overflow, 401/403 auth,
/// 5xx internal. The body-ambiguous cases (429 quota vs rate-limit, 422
/// context-overflow vs invalid) are refined by classify_with_body.
pub(crate) fn classify_status(status: u16, retry_after_ms: Option<u64>) -> ProviderError {
    match status {
        408 => ProviderError::Network,
        429 => ProviderError::RateLimit { retry_after_ms },
        401 | 403 => ProviderError::Auth,
        // No body available here, so the enforced limit is unknown.
        413 | 414 => ProviderError::ContextOverflow {
            enforced_limit: None,
        },
        400 | 404 | 409 | 422 => ProviderError::InvalidRequest(format!("HTTP {status}")),
        500 | 502 | 503 | 504 | 529 => ProviderError::ProviderInternal,
        _ => ProviderError::Unknown(format!("HTTP {status}")),
    }
}

/// Classify an HTTP status using the response body when the status alone is
/// ambiguous. 429 splits into QuotaExceeded (body signals quota/billing
/// exhaustion — NOT retryable, don't burn budget) vs RateLimit (transient —
/// retryable). 400/422/413/414 split into ContextOverflow (body signals
/// context-length exceeded — the compaction trigger) vs InvalidRequest.
/// Substring matching on a lowercased body keeps this dependency-free;
/// providers' phrasing varies across camps, so the patterns are deliberately
/// broad. Empty body falls back to classify_status.
pub(crate) fn classify_with_body(
    status: u16,
    retry_after_ms: Option<u64>,
    body: &str,
) -> ProviderError {
    let lower = body.to_ascii_lowercase();
    match status {
        429 if lower.contains("quota")
            && (lower.contains("exceeded")
                || lower.contains("insufficient")
                || lower.contains("exhausted")) =>
        {
            ProviderError::QuotaExceeded
        }
        400 | 422 | 404 | 409
            if lower.contains("model")
                && (lower.contains("not found")
                    || lower.contains("does not exist")
                    || lower.contains("no such model")
                    || lower.contains("invalid model")) =>
        {
            ProviderError::ModelNotFound(format!("HTTP {status}: {body}"))
        }
        400 | 422 | 413 | 414
            if lower.contains("context")
                && (lower.contains("overflow")
                    || lower.contains("length")
                    || lower.contains("maximum")
                    || lower.contains("exceeded")) =>
        {
            ProviderError::ContextOverflow {
                enforced_limit: extract_context_limit(body),
            }
        }
        _ => classify_status(status, retry_after_ms),
    }
}

/// Parse a Retry-After header. The header has two forms: integer seconds
/// (the common form) or an HTTP-date (the form OpenAI/Cloudflare gateways
/// emit for window-based resets). Integer seconds are honored as-is; an
/// HTTP-date is converted to the milliseconds until that instant (0 when
/// already past). None when the header is absent or unparseable.
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let raw = headers
        .get("retry-after")
        .and_then(|h| h.to_str().ok())?
        .trim();
    // Integer-seconds form.
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(secs * 1000);
    }
    // HTTP-date form: the milliseconds until the named instant. A past date
    // yields 0 (retry now), never negative.
    let date = httpdate::parse_http_date(raw).ok()?;
    let now = std::time::SystemTime::now();
    match date.duration_since(now) {
        Ok(until) => Some(until.as_millis() as u64),
        Err(_) => Some(0),
    }
}

/// Map a reqwest transport error to ProviderError. Timeouts and connection
/// failures are Network (retryable); decode failures become Unknown. Shared
/// because every camp runs over reqwest.
pub(crate) fn map_reqwest_err(e: reqwest::Error) -> ProviderError {
    if e.is_timeout() || e.is_connect() || e.is_request() {
        ProviderError::Network
    } else {
        ProviderError::Unknown(format!("transport: {e}"))
    }
}

/// Extract the enforced context limit a provider names in a context-overflow
/// error body. A provider enforces the real window by rejecting an
/// over-long request, and the rejection body often names the real limit
/// ("this model's maximum context length is 200000", "current length is X
/// while limit is Y"). That number is ground truth, authoritative over the
/// static catalog, so it is carried on the error and recorded for the model
/// to correct an over-estimate at runtime.
///
/// A semantic gate (the body must mention context length / window / limit)
/// prevents parsing an unrelated number; a sane range rejects a misparsed
/// error code or timestamp. Dependency-free substring + digit scan, since
/// providers' phrasing varies across camps and a regex dependency would
/// outweigh the value here.
pub(crate) fn extract_context_limit(body: &str) -> Option<u32> {
    let lower = body.to_ascii_lowercase();
    // Semantic gate: only parse when the body discusses the context window.
    let semantics = [
        "context length",
        "context window",
        "maximum context",
        "context limit",
        "token limit",
        "maximum tokens",
        // Cerebras phrasing: "current length is X while limit is Y".
        "current length",
    ];
    if !semantics.iter().any(|k| lower.contains(k)) {
        return None;
    }
    // "limit is N" first — some providers phrase the enforced limit as
    // "current length is X while limit is Y" and Y is the window.
    if let Some(n) = first_number_after(&lower, "limit is") {
        return clamp_limit(n);
    }
    for anchor in [
        "context length is",
        "context length of",
        "context window is",
        "context window of",
        "maximum context length is",
        "maximum context window",
        "maximum context length",
        "context limit is",
        "token limit is",
    ] {
        if let Some(n) = first_number_after(&lower, anchor) {
            return clamp_limit(n);
        }
    }
    None
}

/// The first integer (allowing comma thousands-separators) that appears
/// within 64 chars after the anchor. None when no digit run follows.
fn first_number_after(haystack: &str, anchor: &str) -> Option<u32> {
    let idx = haystack.find(anchor)?;
    let tail = &haystack[idx + anchor.len()..];
    let mut digits = String::new();
    let mut started = false;
    for c in tail.chars().take(64) {
        if c.is_ascii_digit() {
            started = true;
            digits.push(c);
        } else if c == ',' && started {
            // A thousands separator inside the number — keep scanning digits.
            continue;
        } else if started {
            break;
        }
        // Skip non-digits before the number starts.
    }
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u32>().ok()
}

/// Reject a parsed value outside the range a real context window can take.
/// A 4-digit error code or a 10-digit timestamp survives the semantic gate
/// only when the body also discussed the context window; clamping here
/// discards such a misparse so a wrong number never shrinks the catalog.
fn clamp_limit(n: u32) -> Option<u32> {
    (4_000..=4_000_000).contains(&n).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(raw: &str) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
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
        let empty = reqwest::header::HeaderMap::new();
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
}
