//! Multi-source settings merge: deep-merge user < project < local settings
//! values so a workspace can pin a provider endpoint or model without
//! touching the user's global file. Object keys recurse, non-object values
//! replace. Pure so it is unit-testable without files.

#[cfg(test)]
use crate::ApiKeySource;
use crate::api_key::key_source_from_value;
use serde_json::Map;
use serde_json::Value;
#[cfg(test)]
use std::env::temp_dir;
use std::env::var;
#[cfg(test)]
use std::fs::create_dir_all;
use std::fs::read_to_string;
#[cfg(test)]
use std::fs::remove_dir_all;
#[cfg(test)]
use std::fs::write;
use std::path::Path;
#[cfg(test)]
use std::process::id;

/// Read a settings file as a parsed JSON value. A missing file or invalid
/// JSON yields an empty object (not an error — the caller merges it as a
/// no-op base). Used by the composition root to layer project + local
/// settings over the user settings.
pub fn read_settings_value(path: &Path) -> Value {
    match read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or(Value::Object(Map::new())),
        Err(_) => Value::Object(Map::new()),
    }
}

/// Deep-merge override onto base: for objects, recurse key-by-key
/// (override wins); for any non-object value, override replaces base
/// (user < project < local, later wins). Pure so it is unit-testable.
pub fn merge_json(base: Value, override_: Value) -> Value {
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

/// Settings fields that name where a secret comes from. These are read from
/// the user's own file only and never from the merged value: each one hands
/// control of the api key to whoever wrote it, so honoring a
/// repository-supplied value would let opening a clone divert or execute for
/// the key. A repository file carrying one is ignored with a warning.
///
/// A table rather than a check per field, so adding a secret source cannot
/// silently skip the warning: a new source that is not listed here is a
/// visible omission in this one place.
/// Both fields sit at the top level rather than under provider, grouped by
/// what a repository is allowed to supply rather than by topic. The sibling
/// provider.base_url is merged and honored with a notice; these are refused.
/// Putting a refused field next to merged ones invites the next reader to
/// read it from the merged value, which is exactly the bypass.
const SECRET_SOURCE_FIELDS: &[&str] = &["apiKeyHelper", "keychain"];

/// Load the provider settings from merged settings (user < project < local).
/// When a workspace is given, project-local + local settings layer over the
/// user settings so a team can pin a base_url or model in the repo. The
/// secret source fields are exempt from the merge (see SECRET_SOURCE_FIELDS).
/// Falls back to env for any field the merged settings do not supply.
///
/// The api key itself is not resolved here; the returned key_source names
/// where to get it and the caller obtains it. Running a helper command is a
/// spawn, which needs the chokepoint's timeout and audit policy from a layer
/// this crate cannot reach.
pub fn load_provider_settings(
    workspace: Option<&Path>,
) -> (crate::ProviderSettings, Vec<crate::ConfigWarning>) {
    load_provider_settings_from(&crate::settings_path(), workspace)
}

/// Path-explicit variant of load_provider_settings: the user settings are read
/// from the given file instead of the settings path, so tests isolate
/// without env mutation (same shape as load_retention_from).
pub fn load_provider_settings_from(
    user_settings: &Path,
    workspace: Option<&Path>,
) -> (crate::ProviderSettings, Vec<crate::ConfigWarning>) {
    let mut warnings = Vec::new();
    let user = read_settings_value(user_settings);
    let mut settings = user.clone();
    if let Some(ws) = workspace {
        let project = ws.join(".houyicoder").join("settings.json");
        let local = ws.join(".houyicoder").join("settings.local.json");
        let project_value = read_settings_value(&project);
        let local_value = read_settings_value(&local);
        for field in SECRET_SOURCE_FIELDS {
            if project_value.get(field).is_some() || local_value.get(field).is_some() {
                warnings.push(crate::ConfigWarning {
                    field: (*field).into(),
                    reason: "a project settings file tried to supply your API key — \
                             this is ignored. Put the key in user settings or an \
                             environment variable"
                        .into(),
                });
            }
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
                    "this repo redirects model traffic to {host} — verify that it is a gateway you trust"
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
                var(crate::ENV_DASHSCOPE_BASE_URL).ok(),
                var(crate::ENV_OPENAI_BASE_URL).ok(),
            ])
        })
        .unwrap_or_else(|| crate::DEFAULT_BASE_URL.to_string());
    // The key source is read from the user's own value, never the merged one:
    // the merged value lets a repository-controlled file name the command.
    let key_source = key_source_from_value(&user, &mut warnings);
    let model = settings
        .get("model")
        .and_then(|m| m.get("id"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| crate::DEFAULT_MODEL.to_string());
    (
        crate::ProviderSettings {
            base_url,
            model,
            key_source,
        },
        warnings,
    )
}

#[cfg(test)]
mod tests;
