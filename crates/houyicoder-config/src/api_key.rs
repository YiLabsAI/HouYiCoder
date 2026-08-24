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
    /// A shell command whose stdout is the key.
    Helper(String),
}

/// Read the api key source out of a parsed settings value. Callers hand in the
/// user settings value only: honoring a repository-supplied command would run
/// it on clone open. A blank command reads as no source.
pub(crate) fn key_source_from_value(v: &serde_json::Value) -> Option<ApiKeySource> {
    let helper = v.get("apiKeyHelper")?.as_str()?.trim();
    if helper.is_empty() {
        return None;
    }
    Some(ApiKeySource::Helper(helper.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_helper_command_is_named() {
        let v = serde_json::json!({"apiKeyHelper":"echo test-key-123"});
        assert_eq!(
            key_source_from_value(&v),
            Some(ApiKeySource::Helper("echo test-key-123".into())),
            "the source carries the command, not its output"
        );
    }

    #[test]
    fn test_no_field_no_source() {
        let v = serde_json::json!({"model":{"id":"x"}});
        assert!(
            key_source_from_value(&v).is_none(),
            "no helper => no source"
        );
    }

    #[test]
    fn test_blank_command_no_source() {
        let v = serde_json::json!({"apiKeyHelper":"   "});
        assert!(
            key_source_from_value(&v).is_none(),
            "a cleared field is the same as an absent one"
        );
    }

    #[test]
    fn test_non_string_no_source() {
        let v = serde_json::json!({"apiKeyHelper": 42});
        assert!(
            key_source_from_value(&v).is_none(),
            "a non-string field names no command"
        );
    }
}
