//! Where the api key comes from, as data. The settings file names a source
//! rather than holding the key, so a model switch rewriting settings.json
//! never puts a secret through the atomic temp file.
//!
//! Naming the source is all this does. Obtaining it is a spawn, which needs
//! the chokepoint policy from a layer this leaf cannot reach, so the
//! composition root resolves it.

/// A configured source for the api key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKeySource {
    /// An operating-system keychain item, addressed by service and account.
    Keychain { service: String, account: String },
    /// A shell command whose stdout is the key.
    Helper(String),
}

/// Read the api key source out of a parsed settings value. Callers hand in the
/// user settings value only: both fields hand control of the key to whoever
/// wrote them.
///
/// The keychain wins over the helper when both are set: it is the source that
/// keeps the key out of any file, so a user who configured it meant it, and
/// picking one here means the loser is never even attempted.
///
/// A field present but unusable warns rather than reading as absent -- a
/// keychain entry missing its account is a mistake to report, not a
/// preference to honor.
pub(crate) fn key_source_from_value(
    v: &serde_json::Value,
    warnings: &mut Vec<crate::ConfigWarning>,
) -> Option<ApiKeySource> {
    if let Some(entry) = v.get("keychain") {
        match keychain_source(entry) {
            Ok(source) => return Some(source),
            Err(reason) => warnings.push(crate::ConfigWarning {
                field: "keychain".into(),
                reason,
            }),
        }
    }
    let helper = v.get("apiKeyHelper")?.as_str()?.trim();
    if helper.is_empty() {
        return None;
    }
    Some(ApiKeySource::Helper(helper.to_string()))
}

/// Build a keychain source from the settings entry, or say what is wrong with
/// it. Both fields are required: a lookup by service alone would return
/// whichever account the keychain happens to answer with, so an entry that
/// omits one is ambiguous rather than convenient.
fn keychain_source(entry: &serde_json::Value) -> Result<ApiKeySource, String> {
    let field = |name: &str| {
        entry
            .get(name)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    match (field("service"), field("account")) {
        (Some(service), Some(account)) => Ok(ApiKeySource::Keychain {
            service: service.to_string(),
            account: account.to_string(),
        }),
        (None, Some(_)) => Err("keychain needs a service; ignored".into()),
        (Some(_), None) => Err("keychain needs an account; ignored".into()),
        (None, None) => {
            Err("keychain needs a service and an account as non-empty strings; ignored".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read a source with the warnings discarded, for the cases that assert on
    /// the source alone.
    fn source(v: &serde_json::Value) -> Option<ApiKeySource> {
        key_source_from_value(v, &mut Vec::new())
    }

    /// Read a source and its warnings together.
    fn source_and_warnings(
        v: &serde_json::Value,
    ) -> (Option<ApiKeySource>, Vec<crate::ConfigWarning>) {
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
}
