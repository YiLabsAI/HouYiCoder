//! The apiKeyHelper script: settings.json names a shell command that prints
//! the API key to stdout, so the key never lands in the file — only the
//! command does. This matters once a model switch rewrites settings.json
//! frequently (the atomic temp file is a brief secret-exposure window; a
//! helper keeps the secret out of that path entirely). The shape: exec under
//! a shell, stdout = key, cache the result.

/// Run the apiKeyHelper command named in settings.json + return its stdout
/// (the API key). None when no helper is configured, the field is blank, or
/// the exec fails (the caller falls through to env). Path-explicit so a test
/// points at a temp settings file + a temp command.
///
/// No exec timeout: a hung helper blocks startup. That gap is
/// self-inflicted only - the merged-settings loader hands this reader the
/// user settings value alone, so a repository-controlled file cannot supply
/// a helper; a user who hangs their own helper can see the process and fix
/// their file. A timeout is liveness polish for that residual, deferred
/// until it earns its keep.
///
/// Safety depends on which path the caller passes, and the signature cannot
/// express that; keeping every call site in this crate is what holds the rule.
pub(crate) fn api_key_from_helper(settings_path: &std::path::Path) -> Option<String> {
    let Ok(text) = std::fs::read_to_string(settings_path) else {
        return None;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return None;
    };
    api_key_from_value(&v)
}

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
        let path = std::env::temp_dir().join(format!("helper-read-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"apiKeyHelper":"echo test-key-123"}"#).unwrap();
        assert_eq!(
            api_key_from_helper(&path).as_deref(),
            Some("test-key-123"),
            "helper stdout is the key"
        );
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_helper_none_without_field() {
        let path = std::env::temp_dir().join(format!("helper-none-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"model":{"id":"x"}}"#).unwrap();
        assert!(api_key_from_helper(&path).is_none(), "no helper => None");
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_helper_none_on_failure() {
        let path = std::env::temp_dir().join(format!("helper-fail-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"apiKeyHelper":"exit 1"}"#).unwrap();
        assert!(
            api_key_from_helper(&path).is_none(),
            "a failing helper => None (fall through to env)"
        );
        drop(std::fs::remove_file(&path));
    }
}
