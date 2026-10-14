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
