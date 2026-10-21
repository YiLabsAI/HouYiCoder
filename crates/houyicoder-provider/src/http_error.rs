//! Shared HTTP status classification for providers. Request bodies, stream
//! decoding, cache hints, and usage parsing remain provider-specific.

use houyicoder_protocol::llm::ProviderError;
use reqwest::header::HeaderMap;
use std::time::SystemTime;

/// Classify a status without a response body. Ambiguous responses require
/// classify_with_body to distinguish quota or context errors.
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

/// Refine ambiguous HTTP errors with the response body. Quota exhaustion
/// must not be retried, and context overflow triggers compaction. Other
/// rejected requests retain a bounded description of the server's error.
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
            ProviderError::ModelNotFound(status_detail(status, body))
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
        400 | 404 | 409 | 422 => ProviderError::InvalidRequest(status_detail(status, body)),
        _ => classify_status(status, retry_after_ms),
    }
}

/// Longest endpoint message carried into an error. A provider can echo the
/// whole request back, and the transcript renders this on one row; the cut is
/// marked so a reader knows the message was longer.
const DETAIL_MAX_CHARS: usize = 240;

/// A rejected-request error carrying the endpoint's message when the body has
/// one. The bare status is the fallback, not the norm.
fn status_detail(status: u16, body: &str) -> String {
    match body_detail(body) {
        Some(detail) => format!("HTTP {status}: {detail}"),
        None => format!("HTTP {status}"),
    }
}

/// The endpoint's message from a rejected-request body: its JSON error
/// message when the body is JSON, else the body itself, flattened to one line
/// and cut to a readable length. None when the body carries no text.
fn body_detail(body: &str) -> Option<String> {
    let raw = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            let error = value.get("error")?;
            let message = error
                .get("message")
                .or_else(|| value.get("message"))
                .and_then(|message| message.as_str())
                .or_else(|| error.as_str())?;
            Some(message.to_owned())
        })
        .unwrap_or_else(|| body.to_owned());
    let words: Vec<&str> = raw.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(words.len());
    let mut redact_next = false;
    for word in words {
        if redact_next {
            out.push("[REDACTED]".to_string());
            redact_next = false;
            continue;
        }
        if word.eq_ignore_ascii_case("bearer") || word.to_ascii_lowercase().ends_with(":bearer") {
            out.push(word.to_string());
            redact_next = true;
            continue;
        }
        out.push(redact_word(word));
    }
    Some(cut_detail(&out.join(" ")))
}

/// Redact secret-shaped substrings inside a whitespace-delimited token,
/// covering bare sk-... keys, key=sk-... pairs, and Bearer:sk-... headers.
fn redact_word(word: &str) -> String {
    let lower = word.to_ascii_lowercase();
    if let Some(pos) = lower.find("sk-") {
        let tail = &word[pos + 3..];
        let key_len = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .count();
        if key_len >= 9 {
            let end = pos + 3 + key_len;
            return format!("{}[REDACTED]{}", &word[..pos], &word[end..]);
        }
    }
    for prefix in ["api_key=", "apikey=", "token=", "secret=", "bearer:"] {
        if let Some(pos) = lower.find(prefix) {
            let value_start = pos + prefix.len();
            if value_start < word.len() {
                return format!("{}[REDACTED]", &word[..value_start]);
            }
        }
    }
    word.to_string()
}

/// Cut a message to DETAIL_MAX_CHARS characters, marking the cut.
fn cut_detail(text: &str) -> String {
    if text.chars().count() <= DETAIL_MAX_CHARS {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(DETAIL_MAX_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// Parse a Retry-After header. The header has two forms: integer seconds
/// (the common form) or an HTTP-date (the form OpenAI/Cloudflare gateways
/// emit for window-based resets). Integer seconds are honored as-is; an
/// HTTP-date is converted to the milliseconds until that instant (0 when
/// already past). None when the header is absent or unparseable.
pub(crate) fn parse_retry_after(headers: &HeaderMap) -> Option<u64> {
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
    let now = SystemTime::now();
    match date.duration_since(now) {
        Ok(until) => Some(until.as_millis() as u64),
        Err(_) => Some(0),
    }
}

/// Map a transport error to ProviderError. Timeouts and connection failures
/// are retryable; other failures retain their context as Unknown.
pub(crate) fn map_reqwest_err(e: reqwest::Error) -> ProviderError {
    if e.is_timeout() || e.is_connect() || e.is_request() {
        ProviderError::Network
    } else {
        ProviderError::Unknown(format!("transport: {e}"))
    }
}

/// Extract a provider-enforced context limit for subsequent requests. Only
/// context-related errors qualify; this avoids learning an unrelated number
/// such as a status code or timestamp as the model window.
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
mod tests;
