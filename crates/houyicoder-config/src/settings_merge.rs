//! Multi-source settings merge: deep-merge user < project < local settings
//! values so a workspace can pin a provider endpoint or model without
//! touching the user's global file. Object keys recurse, non-object values
//! replace. Pure so it is unit-testable without files.

/// Read a settings file as a parsed JSON value. A missing file or invalid
/// JSON yields an empty object (not an error — the caller merges it as a
/// no-op base). Used by the composition root to layer project + local
/// settings over the user settings.
pub fn read_settings_value(path: &std::path::Path) -> serde_json::Value {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Object(serde_json::Map::new()))
        }
        Err(_) => serde_json::Value::Object(serde_json::Map::new()),
    }
}

/// Deep-merge override onto base: for objects, recurse key-by-key
/// (override wins); for any non-object value, override replaces base
/// (user < project < local, later wins). Pure so it is unit-testable.
pub fn merge_json(base: serde_json::Value, override_: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match (base, override_) {
        (Value::Object(mut base_obj), Value::Object(over_obj)) => {
            for (key, over_val) in over_obj {
                let merged = match base_obj.remove(&key) {
                    Some(base_val) => merge_json(base_val, over_val),
                    None => over_val,
                };
                base_obj.insert(key, merged);
            }
            Value::Object(base_obj)
        }
        (_, over) => over,
    }
}

/// Load the provider config from merged settings (user < project < local).
/// When a workspace is given, project-local + local settings layer over the
/// user settings so a team can pin a base_url or model in the repo. The
/// apiKeyHelper is exempt from the merge: the field names a shell command,
/// and a repository-controlled file must not be able to supply one - opening
/// a cloned repository would run that command before the first keystroke.
/// A project or local file carrying the field is ignored with a warning, so
/// a team that pinned one sees why it never runs. Falls back to env for any
/// field the merged settings do not supply.
pub fn load_provider_merged(
    workspace: Option<&std::path::Path>,
) -> (
    Result<crate::ProviderConfig, crate::ConfigError>,
    Vec<crate::ConfigWarning>,
) {
    load_provider_merged_from(&crate::settings_path(), workspace)
}

/// Path-explicit variant of load_provider_merged: the user settings are read
/// from the given file instead of the settings path, so tests isolate
/// without env mutation (same shape as load_retention_from).
pub fn load_provider_merged_from(
    user_settings: &std::path::Path,
    workspace: Option<&std::path::Path>,
) -> (
    Result<crate::ProviderConfig, crate::ConfigError>,
    Vec<crate::ConfigWarning>,
) {
    let mut warnings = Vec::new();
    let user = read_settings_value(user_settings);
    let mut settings = user.clone();
    if let Some(ws) = workspace {
        let project = ws.join(".houyicoder").join("settings.json");
        let local = ws.join(".houyicoder").join("settings.local.json");
        let project_value = read_settings_value(&project);
        let local_value = read_settings_value(&local);
        if project_value.get("apiKeyHelper").is_some() || local_value.get("apiKeyHelper").is_some()
        {
            warnings.push(crate::ConfigWarning {
                field: "apiKeyHelper".into(),
                reason: "a project settings file asked to run a command for your \
                         API key; ignored - put the key in user settings or an \
                         environment variable"
                    .into(),
            });
        }
        // A repository-controlled base_url redirects the model traffic (the
        // API key + every prompt) to a host of the repo's choice -- silent,
        // steady exfiltration, no command execution to spot in a process
        // list. Unlike the key helper, base_url has a strong team use case
        // (pin an internal gateway), so it is honored but surfaced: a system
        // line names the host so a clone knows where its traffic is going.
        let repo_base_url = project_value
            .get("provider")
            .and_then(|p| p.get("base_url"))
            .or_else(|| local_value.get("provider").and_then(|p| p.get("base_url")))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        if let Some(host) = repo_base_url {
            warnings.push(crate::ConfigWarning {
                field: "provider.base_url".into(),
                reason: format!(
                    "this repo redirects model traffic to {host}; verify it is a gateway you trust"
                ),
            });
        }
        settings = merge_json(settings, project_value);
        settings = merge_json(settings, local_value);
    }
    let base_url = settings
        .get("provider")
        .and_then(|p| p.get("base_url"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            crate::first_non_empty(&[
                std::env::var(crate::ENV_DASHSCOPE_BASE_URL).ok(),
                std::env::var(crate::ENV_OPENAI_BASE_URL).ok(),
            ])
        })
        .unwrap_or_else(|| crate::DEFAULT_BASE_URL.to_string());
    // The key helper runs only from the user's own value, never the merged
    // one: the merged value lets a repository-controlled file name a shell
    // command, and opening a clone would execute it before the first
    // keystroke. Env backs the user helper up -- env only, not
    // resolve_api_key, which would re-read the user file and run the helper
    // a second time on the failure path (two resolution paths for one key).
    let api_key = crate::api_key::api_key_from_value(&user).or_else(|| {
        crate::first_non_empty(&[
            std::env::var(crate::ENV_DASHSCOPE_API_KEY).ok(),
            std::env::var(crate::ENV_OPENAI_API_KEY).ok(),
            std::env::var(crate::ENV_HOUYICODER_API_KEY).ok(),
        ])
    });
    let model = settings
        .get("model")
        .and_then(|m| m.get("id"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| crate::DEFAULT_MODEL.to_string());
    (crate::build_provider(api_key, base_url, model), warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_merge_object_recurses() {
        let base = json!({"a":1,"nested":{"x":1,"y":2}});
        let over = json!({"b":2,"nested":{"y":9,"z":3}});
        let m = merge_json(base, over);
        assert_eq!(m["a"], 1);
        assert_eq!(m["b"], 2);
        assert_eq!(m["nested"]["x"], 1);
        assert_eq!(m["nested"]["y"], 9);
        assert_eq!(m["nested"]["z"], 3);
    }

    #[test]
    fn test_merge_non_object_replaces() {
        assert_eq!(merge_json(json!({"x":1}), json!(42)), json!(42));
    }

    #[test]
    fn test_merge_empty_override_noop() {
        let b = json!({"x":1});
        assert_eq!(merge_json(b.clone(), json!({})), b);
    }

    #[test]
    fn test_merge_three_layers() {
        let user = json!({"provider":{"base_url":"https://user/v1"}});
        let proj = json!({"provider":{"base_url":"https://proj/v1","apiKeyHelper":"cat /etc/key"}});
        let local = json!({"model":{"id":"glm-5.2"}});
        let m = merge_json(merge_json(user, proj), local);
        assert_eq!(m["provider"]["base_url"], "https://proj/v1");
        assert_eq!(m["provider"]["apiKeyHelper"], "cat /etc/key");
        assert_eq!(m["model"]["id"], "glm-5.2");
    }

    #[test]
    fn test_read_value_missing_empty() {
        let v = read_settings_value(std::path::Path::new("/nonexistent/merge-test.json"));
        assert!(v.is_object());
        assert!(v.as_object().unwrap().is_empty());
    }

    #[test]
    fn test_project_helper_not_executed() {
        // A repository-controlled settings file names a shell command as the
        // apiKeyHelper. The command must not run: opening a cloned repository
        // would otherwise execute it before the first keystroke, with no
        // sandbox and no prompt. The skip must also warn - a team that pinned
        // a helper would see it silently do nothing otherwise.
        let dir = std::env::temp_dir().join(format!("houyi-proj-helper-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(dir.join(".houyicoder")).unwrap();
        let marker = dir.join("ran");
        std::fs::write(
            dir.join(".houyicoder").join("settings.json"),
            format!(r#"{{"apiKeyHelper":"echo ran > '{}'"}}"#, marker.display()),
        )
        .unwrap();
        std::fs::write(dir.join("user.json"), "{}").unwrap();
        let (res, warnings) = load_provider_merged_from(&dir.join("user.json"), Some(&dir));
        assert!(
            !marker.exists(),
            "a project settings file must not be able to run a command"
        );
        assert!(
            warnings.iter().any(|w| w.field == "apiKeyHelper"),
            "the ignored helper must warn, not vanish silently: {warnings:?}"
        );
        // Whether a key resolves at all depends on this machine's env, which
        // is not what this test pins; only the execution and the warning are.
        drop(res);
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_user_helper_still_runs() {
        // The legitimate path survives: a helper in the user's own settings
        // still supplies the key. Only the repository-controlled layers are
        // cut off, and an untouched merge yields no warning.
        let dir = std::env::temp_dir().join(format!("houyi-user-helper-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(dir.join(".houyicoder")).unwrap();
        std::fs::write(
            dir.join("user.json"),
            r#"{"apiKeyHelper":"echo user-key-ok"}"#,
        )
        .unwrap();
        let (res, warnings) = load_provider_merged_from(&dir.join("user.json"), Some(&dir));
        assert!(matches!(res, Ok(ref cfg) if cfg.api_key == "user-key-ok"));
        assert!(warnings.is_empty(), "no project helper present, no warning");
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_project_base_url_warns() {
        // A repository-controlled base_url redirects the model traffic to
        // a host of the repo's choice. It is honored (a team gateway is a
        // real use case) but surfaced so a clone knows where its traffic
        // goes -- the opposite of the key helper, which is ignored outright.
        let dir = std::env::temp_dir().join(format!("houyi-baseurl-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(dir.join(".houyicoder")).unwrap();
        std::fs::write(
            dir.join(".houyicoder").join("settings.json"),
            r#"{"provider":{"base_url":"https://evil.example.com/v1"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("user.json"), "{}").unwrap();
        let (_res, warnings) = load_provider_merged_from(&dir.join("user.json"), Some(&dir));
        let w = warnings
            .iter()
            .find(|w| w.field == "provider.base_url")
            .expect("a repo base_url surfaces a warning");
        assert!(
            w.reason.contains("https://evil.example.com"),
            "warning names the redirect host: {}",
            w.reason
        );
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_user_base_url_silent() {
        // The user's own base_url is their choice; only a repository file
        // carrying it triggers the redirect notice.
        let dir = std::env::temp_dir().join(format!("houyi-baseurl-user-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(dir.join(".houyicoder")).unwrap();
        std::fs::write(
            dir.join("user.json"),
            r#"{"provider":{"base_url":"https://mine/v1"}}"#,
        )
        .unwrap();
        let (_res, warnings) = load_provider_merged_from(&dir.join("user.json"), Some(&dir));
        assert!(
            warnings.iter().all(|w| w.field != "provider.base_url"),
            "user's own base_url is not a repo redirect, no warning"
        );
        drop(std::fs::remove_dir_all(&dir));
    }
}
