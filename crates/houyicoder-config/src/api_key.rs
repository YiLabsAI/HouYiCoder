//! The apiKeyHelper script: settings.json names a shell command that prints
//! the API key to stdout, so the key never lands in the file — only the
//! command does. This matters once a model switch rewrites settings.json
//! frequently (the atomic temp file is a brief secret-exposure window; a
//! helper keeps the secret out of that path entirely). The shape: exec under
//! a shell, stdout = key, cache the result.

/// Extract the apiKeyHelper from a parsed settings value + run it. Callers
/// hand in the user settings value only: the field names a shell command, so
/// a repository-controlled file supplying one would run on clone open.
#[expect(
    clippy::disallowed_methods,
    reason = "user-settings provenance only; a repository cannot supply this string"
)]
pub(crate) fn api_key_from_value(v: &serde_json::Value) -> Option<String> {
    use std::process::{Command, Stdio};
    let helper = v.get("apiKeyHelper")?.as_str()?.trim();
    if helper.is_empty() {
        return None;
    }
    let out = Command::new("sh")
        .arg("-c")
        .arg(helper)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if key.is_empty() { None } else { Some(key) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_helper_reads_script() {
        // The helper string runs under sh; its stdout is the key. The key
        // never lands in the settings file — only the helper command does.
        let v = serde_json::json!({"apiKeyHelper":"echo test-key-123"});
        assert_eq!(
            api_key_from_value(&v).as_deref(),
            Some("test-key-123"),
            "helper stdout is the key"
        );
    }

    #[test]
    fn test_helper_none_without_field() {
        let v = serde_json::json!({"model":{"id":"x"}});
        assert!(api_key_from_value(&v).is_none(), "no helper => None");
    }

    #[test]
    fn test_helper_none_on_failure() {
        let v = serde_json::json!({"apiKeyHelper":"exit 1"});
        assert!(
            api_key_from_value(&v).is_none(),
            "a failing helper => None (fall through to env)"
        );
    }
}
