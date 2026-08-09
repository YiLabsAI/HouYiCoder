//! Secret redaction for the human-facing surfaces (the trajectory pane +
//! the export file). The durable log stays full-fidelity — the model needs
//! real tool I/O to continue the conversation, so redaction runs ONLY at the
//! display / share boundary, never on the stored record.
//!
//! Scope (v1): pattern-based, high-precision / low-false-positive, std-only
//! (no regex dependency). Catches the named secret shapes — key-prefix
//! tokens (sk-, AKIA, ghp_, xoxb-, AIza, ya29., JWT eyJ...) and key=value
//! credentials (password=, token=, api_key=...). Shannon-entropy detection
//! is a v2 refinement: it catches un-prefixed random tokens but
//! false-positives on code (hashes, UUIDs, base64 blobs) without a careful
//! allowlist, so it is deferred until the allowlist is tuned. The v1
//! patterns already cover the common leak shapes a user hits (an .env cat,
//! an AWS credentials read, a token echoed by a tool) with near-zero false
//! positives on normal code.

/// A redaction rule: does this text, starting at byte 0, begin with a secret
/// of this rule's shape? Returns the length of the secret (in bytes) if so.
/// The credential rule is special (it preserves the key name + redacts only
/// the value) so it carries keep_prefix = the bytes of the key+delimiter
/// to keep visible.
struct Rule {
    label: &'static str,
    matches: fn(&str) -> Option<Matched>,
}

struct Matched {
    /// Total bytes the rule consumed from the start of the text.
    total: usize,
    /// Bytes to keep visible before the redaction marker (0 for prefix
    /// rules; the key+delimiter length for credential rules).
    keep_prefix: usize,
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Consume the full run of token chars from the start; return the bytes
/// consumed, or None if fewer than min. A token char is alphanumeric,
/// underscore, or dash.
fn take_token_chars(s: &str, min: usize) -> Option<usize> {
    let mut consumed = 0usize;
    for c in s.chars() {
        if is_token_char(c) {
            consumed += c.len_utf8();
        } else {
            break;
        }
    }
    if consumed >= min {
        Some(consumed)
    } else {
        None
    }
}

// Each rule's matcher. They look at the text from byte 0; the scanner calls
// them at every byte offset.

fn m_openai(s: &str) -> Option<Matched> {
    let rest = s.strip_prefix("sk-")?;
    let n = take_token_chars(rest, 20)?;
    Some(Matched {
        total: 3 + n,
        keep_prefix: 0,
    })
}
fn m_aws(s: &str) -> Option<Matched> {
    let rest = s.strip_prefix("AKIA")?;
    // 16 uppercase alnum.
    let mut consumed = 0;
    for c in rest.chars().take(16) {
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            consumed += c.len_utf8();
        } else {
            break;
        }
    }
    if consumed >= 16 {
        // Keep consuming the full run so the whole key is one marker.
        let mut full = consumed;
        for c in rest[consumed..].chars() {
            if c.is_ascii_uppercase() || c.is_ascii_digit() {
                full += c.len_utf8();
            } else {
                break;
            }
        }
        Some(Matched {
            total: 4 + full,
            keep_prefix: 0,
        })
    } else {
        None
    }
}
fn m_github(s: &str) -> Option<Matched> {
    let rest = s.strip_prefix("gh")?;
    let third = rest.chars().next()?;
    if !matches!(third, 'p' | 'o' | 'u' | 's' | 'r') {
        return None;
    }
    let rest = &rest[third.len_utf8()..];
    let rest = rest.strip_prefix('_')?;
    let n = take_alnum(rest, 36)?;
    Some(Matched {
        total: 2 + 1 + 1 + n,
        keep_prefix: 0,
    })
}
fn m_slack(s: &str) -> Option<Matched> {
    let rest = s.strip_prefix("xox")?;
    let kind = rest.chars().next()?;
    if !matches!(kind, 'b' | 'p') {
        return None;
    }
    let rest = &rest[kind.len_utf8()..];
    let rest = rest.strip_prefix('-')?;
    // 10+ alnum or dash.
    let mut consumed = 0;
    for c in rest.chars() {
        if c.is_ascii_alphanumeric() || c == '-' {
            consumed += c.len_utf8();
        } else {
            break;
        }
    }
    if consumed >= 10 {
        Some(Matched {
            total: 3 + 1 + 1 + consumed,
            keep_prefix: 0,
        })
    } else {
        None
    }
}
fn m_google_api(s: &str) -> Option<Matched> {
    let rest = s.strip_prefix("AIza")?;
    let n = take_token_chars(rest, 35)?;
    Some(Matched {
        total: 4 + n,
        keep_prefix: 0,
    })
}
fn m_google_oauth(s: &str) -> Option<Matched> {
    let rest = s.strip_prefix("ya29.")?;
    let n = take_token_chars(rest, 20)?;
    Some(Matched {
        total: 5 + n,
        keep_prefix: 0,
    })
}
fn m_jwt(s: &str) -> Option<Matched> {
    // eyJ<seg>.<seg>.<seg> — three base64url segments separated by dots,
    // each ≥10 chars, first starts with eyJ.
    let rest = s.strip_prefix("eyJ")?;
    let seg1 = take_base64url(rest, 10)?;
    let rest = rest.get(seg1..)?;
    let rest = rest.strip_prefix('.')?;
    let seg2 = take_base64url(rest, 10)?;
    let rest = rest.get(seg2..)?;
    let rest = rest.strip_prefix('.')?;
    let seg3 = take_base64url(rest, 10)?;
    Some(Matched {
        total: 3 + seg1 + 1 + seg2 + 1 + seg3,
        keep_prefix: 0,
    })
}

fn take_alnum(s: &str, min: usize) -> Option<usize> {
    let mut consumed = 0;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            consumed += c.len_utf8();
        } else {
            break;
        }
    }
    if consumed >= min {
        Some(consumed)
    } else {
        None
    }
}
fn take_base64url(s: &str, min: usize) -> Option<usize> {
    let mut consumed = 0;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            consumed += c.len_utf8();
        } else {
            break;
        }
    }
    if consumed >= min {
        Some(consumed)
    } else {
        None
    }
}

/// key=value credential: password=..., token=..., api_key=..., etc.
/// Case-insensitive key. Keeps the key+delimiter visible, redacts the value.
fn m_credential(s: &str) -> Option<Matched> {
    const KEYS: &[&str] = &[
        "password",
        "passwd",
        "secret",
        "token",
        "apikey",
        "api_key",
        "api-key",
        "accesskey",
        "access_key",
        "access-key",
        "privatekey",
        "private_key",
        "private-key",
        "authtoken",
        "auth_token",
        "auth-token",
    ];
    let lower = s.to_ascii_lowercase();
    let mut key_len = 0;
    for k in KEYS {
        if lower.starts_with(k) {
            key_len = k.len();
            break;
        }
    }
    if key_len == 0 {
        return None;
    }
    let rest = &s[key_len..];
    let bytes = rest.as_bytes();
    let mut p = 0;
    // optional whitespace before the delimiter
    while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
        p += 1;
    }
    // must be : or =
    if p >= bytes.len() || (bytes[p] != b':' && bytes[p] != b'=') {
        return None;
    }
    p += 1; // past the delimiter
    // optional whitespace, then optional opening quote
    while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
        p += 1;
    }
    if p < bytes.len() && (bytes[p] == b'"' || bytes[p] == b'\'') {
        p += 1; // skip the opening quote; value ends at the matching quote
    }
    // value: 8+ non-space/non-quote/non-backslash chars
    let mut value = 0;
    while p + value < bytes.len() {
        let b = bytes[p + value];
        if b == b' ' || b == b'\t' || b == b'"' || b == b'\'' || b == b'\\' || b == b'\n' {
            break;
        }
        value += 1;
    }
    if value < 8 {
        return None;
    }
    Some(Matched {
        total: key_len + p + value,
        keep_prefix: key_len + p,
    })
}

static RULES: &[Rule] = &[
    Rule {
        label: "openai-key",
        matches: m_openai,
    },
    Rule {
        label: "aws-key-id",
        matches: m_aws,
    },
    Rule {
        label: "github-token",
        matches: m_github,
    },
    Rule {
        label: "slack-token",
        matches: m_slack,
    },
    Rule {
        label: "google-api-key",
        matches: m_google_api,
    },
    Rule {
        label: "google-oauth",
        matches: m_google_oauth,
    },
    Rule {
        label: "jwt",
        matches: m_jwt,
    },
    Rule {
        label: "credential",
        matches: m_credential,
    },
];

/// Redact secrets in text, returning a new string with secrets replaced by
/// [REDACTED:label]. Pure + deterministic — no I/O, no mutation of the
/// input. Scans byte-by-byte; at each position tries every rule, and on the
/// first match emits the redaction marker (preserving the key name for
/// credential rules) and advances past the secret. No-match positions pass
/// the char through.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text;
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        let mut matched = None;
        for rule in RULES {
            if let Some(m) = (rule.matches)(rest) {
                matched = Some((rule.label, m));
                break;
            }
        }
        if let Some((label, m)) = matched {
            if m.keep_prefix > 0 {
                // Credential rules: keep_prefix already includes the key +
                // delimiter (+ any quote), so emit it verbatim then the
                // marker — the value is what's hidden.
                out.push_str(&rest[..m.keep_prefix]);
            }
            out.push_str(&format!("[REDACTED:{}]", label));
            i += m.total;
        } else {
            // Advance one char (UTF-8 safe).
            let ch = rest.chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redacts_openai_key() {
        let s = "Authorization: Bearer sk-abcd1234efgh5678ijkl9012mnop3456qrst";
        let r = redact(s);
        assert!(r.contains("[REDACTED:openai-key]"), "got {r}");
        assert!(!r.contains("sk-abcd1234"));
    }

    #[test]
    fn test_redacts_aws_and_github() {
        let s = "aws: AKIAIOSFODNN7EXAMPLE  gh: ghp_abcdefghijklmnopqrstuvwxyz0123456789AB";
        let r = redact(s);
        assert!(r.contains("[REDACTED:aws-key-id]"), "got {r}");
        assert!(r.contains("[REDACTED:github-token]"), "got {r}");
        assert!(!r.contains("AKIAIOSFODNN7"));
        assert!(!r.contains("ghp_abc"));
    }

    #[test]
    fn test_redacts_credential_keeps_name() {
        let s = "DB password=hunter2secretvalue api_key=sk_live_abc12345";
        let r = redact(s);
        assert!(r.contains("password=[REDACTED:credential]"), "got {r}");
        assert!(r.contains("api_key=[REDACTED:credential]"), "got {r}");
        assert!(!r.contains("hunter2secretvalue"));
        assert!(!r.contains("sk_live_abc12345"));
    }

    #[test]
    fn test_redacts_jwt_three_segments() {
        // A bare JWT (no "token:" prefix, which would match the credential
        // rule first) — exercises the jwt rule directly.
        let s = "header eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM.SflKxwRJSMeKKF2QT4f tail";
        let r = redact(s);
        assert!(r.contains("[REDACTED:jwt]"), "got {r}");
        assert!(!r.contains("eyJhbGciOiJIUzI1"));
    }

    #[test]
    fn test_no_false_positive_code() {
        // "AKIA is a word" — AKIA alone (no 16 base32 after) does NOT match
        // the aws-key-id rule (needs 16 uppercase-alnum after), so it stays.
        let s = "let x = 42; // AKIA is a word here";
        let r = redact(s);
        assert!(r.contains("AKIA is a word"), "false positive: {r}");
    }

    #[test]
    fn test_no_match_leaves_unchanged() {
        let s = "a normal line with no secrets whatsoever";
        assert_eq!(redact(s), s);
    }

    #[test]
    fn test_multiple_secrets_one_line() {
        let s = "keys: sk-aaaaaaaaaaaaaaaaaaaaaa and AKIAEXAMPLE12345ABCD";
        let r = redact(s);
        assert!(r.contains("[REDACTED:openai-key]"), "got {r}");
        assert!(r.contains("[REDACTED:aws-key-id]"), "got {r}");
    }
}
