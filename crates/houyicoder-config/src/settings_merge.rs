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
/// user settings so a team can pin a base_url or apiKeyHelper in the repo.
/// Falls back to env for any field the merged settings do not supply.
pub fn load_provider_merged(
    workspace: Option<&std::path::Path>,
) -> Result<crate::ProviderConfig, crate::ConfigError> {
    let mut settings = read_settings_value(&crate::settings_path());
    if let Some(ws) = workspace {
        let project = ws.join(".houyicoder").join("settings.json");
        let local = ws.join(".houyicoder").join("settings.local.json");
        settings = merge_json(settings, read_settings_value(&project));
        settings = merge_json(settings, read_settings_value(&local));
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
    let api_key = crate::api_key::api_key_from_value(&settings).or_else(crate::resolve_api_key);
    let model = settings
        .get("model")
        .and_then(|m| m.get("id"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| crate::DEFAULT_MODEL.to_string());
    crate::build_provider(api_key, base_url, model)
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
}
