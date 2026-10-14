//! Tests for this module.

use super::*;

/// Read a source with the warnings discarded, for the cases that assert on
/// the source alone.
fn source(v: &serde_json::Value) -> Option<ApiKeySource> {
    key_source_from_value(v, &mut Vec::new())
}

/// Read a source and its warnings together.
fn source_and_warnings(v: &serde_json::Value) -> (Option<ApiKeySource>, Vec<crate::ConfigWarning>) {
    let mut warnings = Vec::new();
    let s = key_source_from_value(v, &mut warnings);
    (s, warnings)
}

#[test]
fn test_helper_command_is_named() {
    let v = serde_json::json!({"apiKeyHelper":"echo test-key-123"});
    assert_eq!(
        source(&v),
        Some(ApiKeySource::Helper("echo test-key-123".into())),
        "the source carries the command, not its output"
    );
}

#[test]
fn test_no_field_no_source() {
    let v = serde_json::json!({"model":{"id":"x"}});
    assert!(source(&v).is_none(), "no helper => no source");
}

#[test]
fn test_blank_command_no_source() {
    let v = serde_json::json!({"apiKeyHelper":"   "});
    assert!(
        source(&v).is_none(),
        "a cleared field is the same as an absent one"
    );
}

#[test]
fn test_non_string_no_source() {
    let v = serde_json::json!({"apiKeyHelper": 42});
    assert!(source(&v).is_none(), "a non-string field names no command");
}

#[test]
fn test_keychain_is_named() {
    let v = serde_json::json!({"keychain":{"service":"houyicoder","account":"dashscope"}});
    assert_eq!(
        source(&v),
        Some(ApiKeySource::Keychain {
            service: "houyicoder".into(),
            account: "dashscope".into()
        })
    );
}

/// The keychain wins over a helper configured alongside it, so the helper
/// is never run when both are present.
#[test]
fn test_keychain_beats_helper() {
    let v = serde_json::json!({
        "keychain": {"service":"s","account":"a"},
        "apiKeyHelper": "echo from-helper"
    });
    assert!(matches!(source(&v), Some(ApiKeySource::Keychain { .. })));
}

/// A keychain entry missing a field warns and falls through, rather than
/// silently reading as no source at all.
#[test]
fn test_partial_keychain_warns() {
    let v = serde_json::json!({"keychain":{"service":"s"}});
    let (s, warnings) = source_and_warnings(&v);
    assert!(s.is_none(), "an ambiguous entry names no source");
    assert!(
        warnings.iter().any(|w| w.field == "keychain"),
        "the ignored entry must say why: {warnings:?}"
    );
}

/// A broken keychain entry does not swallow a usable helper: the warning
/// fires and the helper still supplies the key.
#[test]
fn test_broken_keychain_keeps_helper() {
    let v = serde_json::json!({
        "keychain": {"account":"a"},
        "apiKeyHelper": "echo fallback"
    });
    let (s, warnings) = source_and_warnings(&v);
    assert_eq!(s, Some(ApiKeySource::Helper("echo fallback".into())));
    assert!(warnings.iter().any(|w| w.field == "keychain"));
}

#[test]
fn test_blank_keychain_fields_warn() {
    let v = serde_json::json!({"keychain":{"service":"  ","account":""}});
    let (s, warnings) = source_and_warnings(&v);
    assert!(s.is_none());
    assert!(warnings.iter().any(|w| w.field == "keychain"));
}
