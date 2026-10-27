//! Recap settings (recap.enabled, recap.model), read from settings.json.
//!
//! A recap spends a model request while the user is away, so the section is
//! off unless the file switches it on: an upgrade must not start background
//! spending on its own. A file with no recap section is not a warning; a
//! section this build cannot read stays off and says so, because a typo that
//! silently enables paid work is the failure worth surfacing.

use std::fs::read_to_string;
use std::path::Path;

use serde_json::Value;

use crate::ConfigWarning;
use crate::json_type_name;

/// Whether the host may summarize an away session, and which catalog entry
/// the request may use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecapConfig {
    /// Whether the host may generate a recap. False keeps every trigger
    /// path inert, which is the default for a file that never names it.
    pub enabled: bool,
    /// The catalog id the recap request uses. None leaves the choice to the
    /// host, which falls back to a catalog entry declared as the fast tier
    /// and skips the recap when the catalog declares none.
    pub model: Option<String>,
}

/// Pure loader against an explicit path; testable without env mutation.
pub fn load_recap_from(path: &Path) -> (RecapConfig, Vec<ConfigWarning>) {
    let text = match read_to_string(path) {
        Ok(text) => text,
        Err(_) => return (RecapConfig::default(), Vec::new()),
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => {
            return (
                RecapConfig::default(),
                vec![ConfigWarning {
                    field: "<file>".into(),
                    reason: "settings.json is not valid JSON — the recap stays off".into(),
                }],
            );
        }
    };
    let section = match value.get("recap") {
        None | Some(Value::Null) => return (RecapConfig::default(), Vec::new()),
        Some(section) if section.is_object() => section,
        Some(other) => {
            return (
                RecapConfig::default(),
                vec![ConfigWarning {
                    field: "recap".into(),
                    reason: format!(
                        "expected an object, got {} — the recap stays off",
                        json_type_name(other)
                    ),
                }],
            );
        }
    };
    let mut warnings = Vec::new();
    let enabled = extract_enabled(section, &mut warnings);
    let model = extract_model(section, &mut warnings);
    (RecapConfig { enabled, model }, warnings)
}

/// Read the switch. Off unless the file says a JSON true, so every value
/// this build cannot read leaves the host spending nothing.
fn extract_enabled(section: &Value, warnings: &mut Vec<ConfigWarning>) -> bool {
    match section.get("enabled") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(enabled)) => *enabled,
        Some(other) => {
            warnings.push(ConfigWarning {
                field: "recap.enabled".into(),
                reason: format!(
                    "expected a boolean, got {} — the recap stays off",
                    json_type_name(other)
                ),
            });
            false
        }
    }
}

/// Read the model id. A malformed value is dropped rather than guessed: the
/// host then falls back to the declared fast tier, which is a catalog fact
/// rather than a guess from a model's name.
fn extract_model(section: &Value, warnings: &mut Vec<ConfigWarning>) -> Option<String> {
    match section.get("model") {
        None | Some(Value::Null) => None,
        Some(Value::String(model)) if !model.trim().is_empty() => Some(model.clone()),
        Some(Value::String(_)) => {
            warnings.push(ConfigWarning {
                field: "recap.model".into(),
                reason: "the value is empty — the host chooses the model".into(),
            });
            None
        }
        Some(other) => {
            warnings.push(ConfigWarning {
                field: "recap.model".into(),
                reason: format!(
                    "expected a string, got {} — the host chooses the model",
                    json_type_name(other)
                ),
            });
            None
        }
    }
}

#[cfg(test)]
mod tests;
